// MVT (Mapbox Vector Tile) protobuf encoder.
//
// Hand-rolled protobuf encoding — the MVT schema is simple enough that codegen
// is unnecessary. Produces spec-compliant tiles with extent=4096.

// FxHashMap (rustc-hash): non-cryptographic hash ~3× faster than std SipHash for
// small keys. Safe here because keys are short strings and interned integers — no
// adversarial input. Tradeoff: weaker collision resistance (irrelevant for tile
// encoding). Already a transitive dependency via roaring. To revert, swap back to
// std::collections::HashMap and remove the rustc-hash direct dependency.
use protohoggr::{
    encode_bytes_field_always, encode_packed_uint32, encode_tag, encode_varint,
    encode_varint_field_always, zigzag_encode_64, WIRE_32BIT, WIRE_64BIT, WIRE_VARINT,
};
use rustc_hash::FxHashMap;
use std::hash::{Hash, Hasher};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum GeomType {
    Point = 1,
    LineString = 2,
    Polygon = 3,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum Value {
    String(String),
    Float(f32),
    Double(f64),
    Int(i64),
    UInt(u64),
    SInt(i64),
    Bool(bool),
}

// Manual PartialEq+Eq+Hash: can't derive because f32/f64 don't impl Eq/Hash.
// Using to_bits() for floats gives bitwise equality, which is correct for
// MVT value interning (we want exact dedup, not fuzzy float comparison).
// This also makes PartialEq consistent with Hash (both use to_bits()),
// satisfying the Eq contract even for NaN values.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits(),
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::UInt(a), Value::UInt(b)) => a == b,
            (Value::SInt(a), Value::SInt(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Value::String(s) => s.hash(state),
            Value::Float(f) => f.to_bits().hash(state),
            Value::Double(f) => f.to_bits().hash(state),
            Value::Int(i) | Value::SInt(i) => i.hash(state),
            Value::UInt(u) => u.hash(state),
            Value::Bool(b) => b.hash(state),
        }
    }
}

pub struct Feature {
    pub id: Option<u64>,
    pub geom_type: GeomType,
    pub geometry: Vec<u32>,
    pub tags: Vec<(u16, u16)>,
}
const _: () = assert!(std::mem::size_of::<Feature>() == 72);

/// Reusable scratch buffers for MVT encoding, avoiding per-feature allocations.
pub struct EncodeScratch {
    layer_buf: Vec<u8>,
    feat_buf: Vec<u8>,
    val_buf: Vec<u8>,
    packed: Vec<u8>,
    tag_vals: Vec<u32>,
}

impl EncodeScratch {
    pub fn new() -> Self {
        Self {
            layer_buf: Vec::new(),
            feat_buf: Vec::new(),
            val_buf: Vec::new(),
            packed: Vec::new(),
            tag_vals: Vec::new(),
        }
    }
}

/// Reusable scratch buffers for geometry merging, avoiding per-tile allocation.
/// Created once per thread via `thread_local!`, reused across all tiles on that worker.
pub struct MergeScratch {
    indices: Vec<usize>,
    geom: Vec<u32>,
}

impl MergeScratch {
    pub fn new() -> Self {
        Self {
            indices: Vec::new(),
            geom: Vec::new(),
        }
    }
}

pub struct LayerBuilder {
    name: String,
    features: Vec<Feature>,
    keys: Vec<String>,
    key_map: FxHashMap<String, u16>,
    values: Vec<Value>,
    value_map: FxHashMap<Value, u16>,
    string_value_map: FxHashMap<String, u16>,
}

// ---------------------------------------------------------------------------
// LayerBuilder
// ---------------------------------------------------------------------------

impl LayerBuilder {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            features: Vec::new(),
            keys: Vec::new(),
            key_map: FxHashMap::default(),
            values: Vec::new(),
            value_map: FxHashMap::default(),
            string_value_map: FxHashMap::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn intern_key(&mut self, key: &str) -> u16 {
        if let Some(&idx) = self.key_map.get(key) {
            return idx;
        }
        let idx = self.keys.len().min(u16::MAX as usize) as u16;
        let owned = key.to_string();
        self.key_map.insert(owned.clone(), idx);
        self.keys.push(owned);
        idx
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn intern_value(&mut self, val: Value) -> u16 {
        // Route string values through the borrowed-str path to keep
        // string_value_map and value_map consistent.
        if let Value::String(ref s) = val {
            return self.intern_string_value(s);
        }
        if let Some(&idx) = self.value_map.get(&val) {
            return idx;
        }
        let idx = self.values.len().min(u16::MAX as usize) as u16;
        self.value_map.insert(val.clone(), idx);
        self.values.push(val);
        idx
    }

    /// Intern a string value by borrowed `&str`, avoiding allocation on cache hit.
    /// Same pattern as `intern_key`: `HashMap<String, u16>` supports `get(&str)`
    /// because `String: Borrow<str>`.
    // Two owned Strings needed on miss: HashMap key + Value enum. Can't avoid
    // without redesigning the interning data structure.
    #[allow(clippy::cast_possible_truncation)]
    pub fn intern_string_value(&mut self, s: &str) -> u16 {
        if let Some(&idx) = self.string_value_map.get(s) {
            return idx;
        }
        let idx = self.values.len().min(u16::MAX as usize) as u16;
        let owned = s.to_string();
        self.values.push(Value::String(owned.clone()));
        self.string_value_map.insert(owned, idx);
        idx
    }

    pub fn add_feature(&mut self, feature: Feature) {
        self.features.push(feature);
    }

    /// Reclaim feature buffers and clear interning state, keeping allocated capacity.
    /// Used by assembly `thread_local!` to reuse `LayerBuilder` across tiles — the
    /// HashMap bucket arrays survive, avoiding re-allocation on the next tile.
    pub fn prepare_for_reuse(
        &mut self,
        geom_pool: &mut Vec<Vec<u32>>,
        tags_pool: &mut Vec<Vec<(u16, u16)>>,
    ) {
        for f in self.features.drain(..) {
            geom_pool.push(f.geometry);
            tags_pool.push(f.tags);
        }
        self.keys.clear();
        self.key_map.clear();
        self.values.clear();
        self.value_map.clear();
        self.string_value_map.clear();
    }

    fn encode(&self, buf: &mut Vec<u8>, s: &mut EncodeScratch) {
        s.layer_buf.clear();

        // field 15: version = 2
        encode_varint_field_always(&mut s.layer_buf, 15, 2);
        // field 1: name
        encode_bytes_field_always(&mut s.layer_buf, 1, self.name.as_bytes());
        // field 5: extent = 4096
        encode_varint_field_always(&mut s.layer_buf, 5, 4096);

        // field 2: features
        for f in &self.features {
            s.feat_buf.clear();
            if let Some(id) = f.id {
                encode_varint_field_always(&mut s.feat_buf, 1, id);
            }
            if !f.tags.is_empty() {
                s.tag_vals.clear();
                s.tag_vals.extend(
                    f.tags
                        .iter()
                        .flat_map(|&(k, v)| [u32::from(k), u32::from(v)]),
                );
                encode_packed_uint32(&mut s.feat_buf, &mut s.packed, 2, &s.tag_vals);
            }
            encode_varint_field_always(&mut s.feat_buf, 3, f.geom_type as u64);
            if !f.geometry.is_empty() {
                encode_packed_uint32(&mut s.feat_buf, &mut s.packed, 4, &f.geometry);
            }
            encode_bytes_field_always(&mut s.layer_buf, 2, &s.feat_buf);
        }

        // field 3: keys
        for k in &self.keys {
            encode_bytes_field_always(&mut s.layer_buf, 3, k.as_bytes());
        }

        // field 4: values
        for v in &self.values {
            s.val_buf.clear();
            encode_value(&mut s.val_buf, v);
            encode_bytes_field_always(&mut s.layer_buf, 4, &s.val_buf);
        }

        // Write as field 3 (Tile.layers) length-delimited
        encode_bytes_field_always(buf, 3, &s.layer_buf);
    }
}

// ---------------------------------------------------------------------------
// Top-level encoder
// ---------------------------------------------------------------------------

#[cfg(test)]
pub fn encode_tile(layers: &[&LayerBuilder]) -> Vec<u8> {
    let mut scratch = EncodeScratch::new();
    let mut buf = Vec::with_capacity(1 << 16);
    encode_tile_into(&mut buf, layers, &mut scratch);
    buf
}

/// Encode layers into a caller-provided buffer, avoiding per-tile allocation.
#[hotpath::measure]
pub fn encode_tile_into(buf: &mut Vec<u8>, layers: &[&LayerBuilder], scratch: &mut EncodeScratch) {
    buf.clear();
    for layer in layers {
        if !layer.is_empty() {
            layer.encode(buf, scratch);
        }
    }
}

// ---------------------------------------------------------------------------
// Geometry command encoding
// ---------------------------------------------------------------------------

pub fn encode_point(buf: &mut Vec<u32>, x: i32, y: i32) {
    buf.clear();
    buf.push(command(1, 1)); // MoveTo, count=1
    buf.push(zigzag(x));
    buf.push(zigzag(y));
}

pub fn encode_linestring(buf: &mut Vec<u32>, coords: &[(i32, i32)]) {
    buf.clear();
    if coords.len() < 2 {
        return;
    }
    buf.reserve(3 + (coords.len() - 1) * 2);
    // MoveTo first point
    buf.push(command(1, 1));
    buf.push(zigzag(coords[0].0));
    buf.push(zigzag(coords[0].1));
    // LineTo remaining points, skipping consecutive duplicates
    let lineto_pos = buf.len();
    buf.push(0); // placeholder for LineTo command (patched below)
    let mut cx = coords[0].0;
    let mut cy = coords[0].1;
    let mut count = 0u32;
    for &(x, y) in &coords[1..] {
        if x == cx && y == cy {
            continue;
        }
        buf.push(zigzag(x - cx));
        buf.push(zigzag(y - cy));
        cx = x;
        cy = y;
        count += 1;
    }
    if count < 1 {
        buf.clear(); // degenerate: all points collapsed to one
        return;
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        buf[lineto_pos] = command(2, count);
    }
}

pub fn encode_polygon(buf: &mut Vec<u32>, rings: &[&[(i32, i32)]]) {
    buf.clear();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    for ring in rings {
        if ring.len() < 4 {
            continue;
        }
        let points = &ring[..ring.len() - 1];
        // Save state in case we need to discard a degenerate ring
        let save_len = buf.len();
        let save_cx = cx;
        let save_cy = cy;
        // MoveTo first point
        buf.push(command(1, 1));
        buf.push(zigzag(points[0].0 - cx));
        buf.push(zigzag(points[0].1 - cy));
        cx = points[0].0;
        cy = points[0].1;
        // LineTo remaining, skipping consecutive duplicates
        let lineto_pos = buf.len();
        buf.push(0); // placeholder for LineTo command
        let mut count = 0u32;
        for &(x, y) in &points[1..] {
            if x == cx && y == cy {
                continue;
            }
            buf.push(zigzag(x - cx));
            buf.push(zigzag(y - cy));
            cx = x;
            cy = y;
            count += 1;
        }
        if count < 2 {
            // Degenerate ring after dedup (< 3 unique points) — discard
            buf.truncate(save_len);
            cx = save_cx;
            cy = save_cy;
            continue;
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            buf[lineto_pos] = command(2, count);
        }
        // ClosePath
        buf.push(command(7, 1));
        // After ClosePath, cursor returns to the MoveTo position
        cx = points[0].0;
        cy = points[0].1;
    }
}

fn encode_value(buf: &mut Vec<u8>, val: &Value) {
    match val {
        Value::String(s) => encode_bytes_field_always(buf, 1, s.as_bytes()),
        Value::Float(f) => {
            encode_tag(buf, 2, WIRE_32BIT);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::Double(d) => {
            encode_tag(buf, 3, WIRE_64BIT);
            buf.extend_from_slice(&d.to_le_bytes());
        }
        #[allow(clippy::cast_sign_loss)]
        Value::Int(i) => encode_varint_field_always(buf, 4, *i as u64),
        Value::UInt(u) => encode_varint_field_always(buf, 5, *u),
        Value::SInt(i) => {
            encode_tag(buf, 6, WIRE_VARINT);
            encode_varint(buf, zigzag_encode_64(*i));
        }
        Value::Bool(b) => encode_varint_field_always(buf, 7, u64::from(*b)),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[inline]
fn zigzag(v: i32) -> u32 {
    #[allow(clippy::cast_sign_loss)]
    { ((v << 1) ^ (v >> 31)) as u32 }
}

#[inline]
fn command(id: u32, count: u32) -> u32 {
    id | (count << 3)
}

#[inline]
fn decode_zigzag(v: u32) -> i32 {
    #[allow(clippy::cast_possible_wrap)]
    { ((v >> 1) as i32) ^ (-((v & 1) as i32)) }
}

/// Append a source MVT geometry command stream to a destination buffer,
/// adjusting delta encoding so the commands are relative to the running
/// cursor (`cx`, `cy`). This allows multiple independently-encoded
/// geometries to be concatenated into a valid multi-geometry.
fn append_geometry(dest: &mut Vec<u32>, src: &[u32], cx: &mut i32, cy: &mut i32) {
    let mut last_move_x: i32 = 0;
    let mut last_move_y: i32 = 0;
    // The source feature was encoded assuming cursor starts at (0,0).
    // Track the source's absolute cursor so we can re-encode deltas
    // relative to our running destination cursor.
    let mut src_cx: i32 = 0;
    let mut src_cy: i32 = 0;
    let mut i = 0;
    while i < src.len() {
        let cmd = src[i];
        let cmd_id = cmd & 0x7;
        let cmd_count = cmd >> 3;
        i += 1;

        match cmd_id {
            1 | 2 => {
                // MoveTo or LineTo
                let cmd_pos = dest.len();
                dest.push(cmd);
                let mut actual_count = 0u32;
                for _ in 0..cmd_count {
                    if i + 1 >= src.len() {
                        break;
                    }
                    actual_count += 1;
                    let dx = decode_zigzag(src[i]);
                    let dy = decode_zigzag(src[i + 1]);
                    // Absolute position in source coordinate space
                    src_cx += dx;
                    src_cy += dy;
                    // Delta relative to our running cursor
                    dest.push(zigzag(src_cx - *cx));
                    dest.push(zigzag(src_cy - *cy));
                    *cx = src_cx;
                    *cy = src_cy;
                    if cmd_id == 1 {
                        last_move_x = src_cx;
                        last_move_y = src_cy;
                    }
                    i += 2;
                }
                // Patch command header with actual count if truncated.
                if actual_count != cmd_count {
                    dest[cmd_pos] = (actual_count << 3) | cmd_id;
                }
            }
            7 => {
                // ClosePath
                dest.push(cmd);
                *cx = last_move_x;
                *cy = last_move_y;
            }
            _ => {
                // Unknown command, copy as-is
                dest.push(cmd);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-geometry merging
// ---------------------------------------------------------------------------

impl LayerBuilder {
    /// Merge features that share the same geometry type and identical
    /// attribute tags into a single multi-geometry feature. This reduces
    /// feature counts in the encoded tile without losing any visual
    /// information. Point features are skipped (not merged).
    ///
    /// Uses sort + scan instead of HashMap to avoid per-feature tag cloning.
    /// Sort indices by (geom_type, tags), scan for consecutive runs, merge
    /// each run in-place via scratch buffer + swap. Tombstones secondaries
    /// with empty geometry, then retains non-tombstone features.
    #[hotpath::measure]
    pub fn merge_same_attr_geometries(
        &mut self,
        scratch: &mut MergeScratch,
        geom_pool: &mut Vec<Vec<u32>>,
        tags_pool: &mut Vec<Vec<(u16, u16)>>,
    ) {
        // Build sorted index of non-Point features.
        scratch.indices.clear();
        for (i, f) in self.features.iter().enumerate() {
            if f.geom_type != GeomType::Point {
                scratch.indices.push(i);
            }
        }
        if scratch.indices.len() < 2 {
            return;
        }

        // Sort by (geom_type, tags). Tags are deterministic from shortbread
        // matching — no normalization needed. Rust's sort is stable (Timsort).
        scratch.indices.sort_by(|&a, &b| {
            let fa = &self.features[a];
            let fb = &self.features[b];
            fa.geom_type.cmp(&fb.geom_type)
                .then_with(|| fa.tags.cmp(&fb.tags))
        });

        // Scan for consecutive runs and merge each run in-place.
        let mut any_merged = false;
        let mut i = 0;
        while i < scratch.indices.len() {
            let mut j = i + 1;
            let fi = scratch.indices[i];
            while j < scratch.indices.len() {
                let fj = scratch.indices[j];
                if self.features[fj].geom_type != self.features[fi].geom_type
                    || self.features[fj].tags != self.features[fi].tags
                {
                    break;
                }
                j += 1;
            }
            if j - i >= 2 {
                any_merged = true;
                // Concatenate all geometries into scratch buffer
                scratch.geom.clear();
                let mut cx: i32 = 0;
                let mut cy: i32 = 0;
                for k in i..j {
                    let idx = scratch.indices[k];
                    append_geometry(&mut scratch.geom, &self.features[idx].geometry, &mut cx, &mut cy);
                }
                // Swap merged geometry into first feature
                let first = scratch.indices[i];
                std::mem::swap(&mut self.features[first].geometry, &mut scratch.geom);
                self.features[first].id = None;
                // Reclaim secondary features' Vecs into pools (mem::take leaves
                // zero-capacity Vecs so retain can identify dead features).
                for k in (i + 1)..j {
                    let idx = scratch.indices[k];
                    geom_pool.push(std::mem::take(&mut self.features[idx].geometry));
                    tags_pool.push(std::mem::take(&mut self.features[idx].tags));
                }
            }
            i = j;
        }

        if any_merged {
            // Remove dead features (zero-capacity Vecs from mem::take).
            // Uses is_empty() as tombstone proxy — safe because all pipeline-emitted
            // features have non-empty geometry (enforced at all emit call sites).
            self.features.retain(|f| !f.geometry.is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// Test accessors (expose private fields for cross-module test assertions)
// ---------------------------------------------------------------------------

#[cfg(test)]
impl LayerBuilder {
    /// Number of features added to this layer.
    pub fn test_feature_count(&self) -> usize {
        self.features.len()
    }

    /// Access the i-th feature (panics if out of range).
    pub fn test_feature(&self, i: usize) -> &Feature {
        &self.features[i]
    }

    /// Resolve a key index to its string.
    pub fn test_key(&self, idx: u16) -> &str {
        &self.keys[idx as usize]
    }

    /// Resolve a value index to its `Value`.
    pub fn test_value(&self, idx: u16) -> &Value {
        &self.values[idx as usize]
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use protohoggr::{Cursor, WIRE_LEN};

    #[derive(Debug, PartialEq, Eq)]
    struct ParsedFixtureTile {
        layer_name: String,
        layer_version: u64,
        layer_extent: Option<u64>,
        key: String,
        string_value: String,
        feature_id: Option<u64>,
        feature_type: u64,
        feature_tags: Vec<u32>,
        feature_geometry: Vec<u32>,
    }

    fn parse_single_layer_single_feature(bytes: &[u8]) -> ParsedFixtureTile {
        let mut tile_cursor = Cursor::new(bytes);
        let mut layer_buf: Option<&[u8]> = None;
        while let Ok(Some((field, wire))) = tile_cursor.read_tag() {
            if field == 3 && wire == WIRE_LEN {
                layer_buf = Some(tile_cursor.read_len_delimited().unwrap());
                break;
            }
            tile_cursor.skip_field(wire).unwrap();
        }
        let layer_buf = layer_buf.expect("tile must contain one layer");

        let mut layer_name = String::new();
        let mut layer_version = 0u64;
        let mut layer_extent: Option<u64> = None;
        let mut key = String::new();
        let mut string_value = String::new();
        let mut feature_id = None;
        let mut feature_type = 0u64;
        let mut feature_tags = Vec::new();
        let mut feature_geometry = Vec::new();

        let mut layer_cursor = Cursor::new(layer_buf);
        while let Ok(Some((field, wire))) = layer_cursor.read_tag() {
            match (field, wire) {
                (15, WIRE_VARINT) => layer_version = layer_cursor.read_varint().unwrap(),
                (1, WIRE_LEN) => {
                    layer_name = String::from_utf8_lossy(layer_cursor.read_len_delimited().unwrap()).to_string();
                }
                (5, WIRE_VARINT) => layer_extent = Some(layer_cursor.read_varint().unwrap()),
                (3, WIRE_LEN) => {
                    key = String::from_utf8_lossy(layer_cursor.read_len_delimited().unwrap()).to_string();
                }
                (4, WIRE_LEN) => {
                    let value_msg = layer_cursor.read_len_delimited().unwrap();
                    let mut vcur = Cursor::new(value_msg);
                    while let Ok(Some((vf, vw))) = vcur.read_tag() {
                        if vf == 1 && vw == WIRE_LEN {
                            string_value =
                                String::from_utf8_lossy(vcur.read_len_delimited().unwrap()).to_string();
                            break;
                        }
                        vcur.skip_field(vw).unwrap();
                    }
                }
                (2, WIRE_LEN) => {
                    let feature_msg = layer_cursor.read_len_delimited().unwrap();
                    let mut fcur = Cursor::new(feature_msg);
                    while let Ok(Some((ff, fw))) = fcur.read_tag() {
                        match (ff, fw) {
                            (1, WIRE_VARINT) => feature_id = Some(fcur.read_varint().unwrap()),
                            (2, WIRE_LEN) => {
                                let packed = fcur.read_len_delimited().unwrap();
                                let mut p = Cursor::new(packed);
                                while !p.is_empty() {
                                    feature_tags.push(
                                        u32::try_from(p.read_varint().unwrap())
                                            .expect("fixture tags must fit in u32"),
                                    );
                                }
                            }
                            (3, WIRE_VARINT) => feature_type = fcur.read_varint().unwrap(),
                            (4, WIRE_LEN) => {
                                let packed = fcur.read_len_delimited().unwrap();
                                let mut p = Cursor::new(packed);
                                while !p.is_empty() {
                                    feature_geometry.push(
                                        u32::try_from(p.read_varint().unwrap())
                                            .expect("fixture geometry must fit in u32"),
                                    );
                                }
                            }
                            _ => fcur.skip_field(fw).unwrap(),
                        }
                    }
                }
                _ => layer_cursor.skip_field(wire).unwrap(),
            }
        }

        ParsedFixtureTile {
            layer_name,
            layer_version,
            layer_extent,
            key,
            string_value,
            feature_id,
            feature_type,
            feature_tags,
            feature_geometry,
        }
    }

    #[test]
    fn test_zigzag() {
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
        assert_eq!(zigzag(-2), 3);
        assert_eq!(zigzag(2), 4);
    }

    #[test]
    fn test_varint_encoding() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, 0);
        assert_eq!(buf, [0]);

        buf.clear();
        encode_varint(&mut buf, 1);
        assert_eq!(buf, [1]);

        buf.clear();
        encode_varint(&mut buf, 127);
        assert_eq!(buf, [127]);

        buf.clear();
        encode_varint(&mut buf, 128);
        assert_eq!(buf, [0x80, 0x01]);

        buf.clear();
        encode_varint(&mut buf, 300);
        assert_eq!(buf, [0xAC, 0x02]);
    }

    #[test]
    fn test_command_encoding() {
        assert_eq!(command(1, 1), 9);  // MoveTo, count=1
        assert_eq!(command(2, 3), 26); // LineTo, count=3
        assert_eq!(command(7, 1), 15); // ClosePath, count=1
    }

    #[test]
    fn test_encode_point() {
        let mut cmds = Vec::new();
        encode_point(&mut cmds, 25, 17);
        assert_eq!(cmds, vec![
            9,  // MoveTo, count=1
            50, // zigzag(25) = 50
            34, // zigzag(17) = 34
        ]);
    }

    #[test]
    fn test_encode_linestring() {
        let coords = [(2, 1), (4, 3), (6, 5)];
        let mut cmds = Vec::new();
        encode_linestring(&mut cmds, &coords);
        assert_eq!(cmds, vec![
            9,  // MoveTo count=1
            4,  // zigzag(2)
            2,  // zigzag(1)
            18, // LineTo count=2
            4,  // zigzag(4-2=2)
            4,  // zigzag(3-1=2)
            4,  // zigzag(6-4=2)
            4,  // zigzag(5-3=2)
        ]);
    }

    #[test]
    fn test_encode_polygon() {
        // Simple square: (0,0) -> (10,0) -> (10,10) -> (0,10) -> (0,0)
        let ring = [(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
        let mut cmds = Vec::new();
        encode_polygon(&mut cmds, &[&ring]);
        // MoveTo(0,0): cmd=9, zigzag(0)=0, zigzag(0)=0
        // LineTo 3 points: cmd=26
        //   delta(10,0): 20, 0
        //   delta(0,10): 0, 20
        //   delta(-10,0): 19, 0
        // ClosePath: cmd=15
        assert_eq!(cmds, vec![9, 0, 0, 26, 20, 0, 0, 20, 19, 0, 15]);
    }

    #[test]
    fn test_layer_builder_key_interning() {
        let mut layer = LayerBuilder::new("test");
        let k1 = layer.intern_key("name");
        let k2 = layer.intern_key("highway");
        let k3 = layer.intern_key("name");
        assert_eq!(k1, 0);
        assert_eq!(k2, 1);
        assert_eq!(k3, 0); // dedup
    }

    #[test]
    fn test_layer_builder_value_interning() {
        let mut layer = LayerBuilder::new("test");
        let v1 = layer.intern_value(Value::String("residential".into()));
        let v2 = layer.intern_value(Value::Bool(true));
        let v3 = layer.intern_value(Value::String("residential".into()));
        assert_eq!(v1, 0);
        assert_eq!(v2, 1);
        assert_eq!(v3, 0); // dedup
    }

    #[test]
    fn test_encode_tile_produces_bytes() {
        let mut layer = LayerBuilder::new("streets");
        let ki = layer.intern_key("kind");
        let vi = layer.intern_value(Value::String("motorway".into()));
        let mut geom = Vec::new();
        encode_linestring(&mut geom, &[(100, 200), (300, 400)]);
        layer.add_feature(Feature {
            id: Some(42),
            geom_type: GeomType::LineString,
            geometry: geom,
            tags: vec![(ki, vi)],
        });

        let buf = encode_tile(&[&layer]);
        assert!(!buf.is_empty());
        // Verify outer field tag: field 3, wire type 2
        assert_eq!(buf[0], (3 << 3) | 2);
    }

    #[test]
    fn test_encode_empty_tile() {
        let layer = LayerBuilder::new("empty");
        let buf = encode_tile(&[&layer]);
        assert!(buf.is_empty());
    }

    fn assert_matches_mvt_fixture(fixture_id: &str, geom_type: GeomType, geometry: &[u32]) {
        let mut layer = LayerBuilder::new("hello");
        let key_idx = layer.intern_key("hello");
        let val_idx = layer.intern_value(Value::String("world".to_string()));
        layer.add_feature(Feature {
            id: Some(1),
            geom_type,
            geometry: geometry.to_vec(),
            tags: vec![(key_idx, val_idx)],
        });
        let encoded = encode_tile(&[&layer]);

        let expected: &[u8] = match fixture_id {
            "017" => include_bytes!("../tests/fixtures/mvt_fixtures/017/tile.mvt"),
            "018" => include_bytes!("../tests/fixtures/mvt_fixtures/018/tile.mvt"),
            "019" => include_bytes!("../tests/fixtures/mvt_fixtures/019/tile.mvt"),
            "020" => include_bytes!("../tests/fixtures/mvt_fixtures/020/tile.mvt"),
            "021" => include_bytes!("../tests/fixtures/mvt_fixtures/021/tile.mvt"),
            "022" => include_bytes!("../tests/fixtures/mvt_fixtures/022/tile.mvt"),
            _ => panic!("unknown fixture id: {fixture_id}"),
        };

        let expected_parsed = parse_single_layer_single_feature(expected);
        let actual_parsed = parse_single_layer_single_feature(&encoded);

        // Upstream fixtures sometimes omit default-encoded fields like extent.
        assert_eq!(actual_parsed.layer_name, expected_parsed.layer_name, "fixture {fixture_id} layer_name");
        assert_eq!(actual_parsed.layer_version, expected_parsed.layer_version, "fixture {fixture_id} version");
        assert_eq!(actual_parsed.key, expected_parsed.key, "fixture {fixture_id} key");
        assert_eq!(actual_parsed.string_value, expected_parsed.string_value, "fixture {fixture_id} value");
        assert_eq!(actual_parsed.feature_id, expected_parsed.feature_id, "fixture {fixture_id} id");
        assert_eq!(actual_parsed.feature_type, expected_parsed.feature_type, "fixture {fixture_id} type");
        assert_eq!(actual_parsed.feature_tags, expected_parsed.feature_tags, "fixture {fixture_id} tags");
        assert_eq!(
            actual_parsed.feature_geometry, expected_parsed.feature_geometry,
            "fixture {fixture_id} geometry"
        );

        // Our encoder always writes extent=4096 for spec compliance.
        assert_eq!(actual_parsed.layer_extent, Some(4096), "fixture {fixture_id} extent");
    }

    #[test]
    fn conformance_fixture_017_valid_point_geometry() {
        // mapbox/mvt-fixtures#017
        assert_matches_mvt_fixture("017", GeomType::Point, &[9, 50, 34]);
    }

    #[test]
    fn conformance_fixture_018_valid_linestring_geometry() {
        // mapbox/mvt-fixtures#018
        assert_matches_mvt_fixture("018", GeomType::LineString, &[9, 4, 4, 18, 0, 16, 16, 0]);
    }

    #[test]
    fn conformance_fixture_019_valid_polygon_geometry() {
        // mapbox/mvt-fixtures#019
        assert_matches_mvt_fixture("019", GeomType::Polygon, &[9, 6, 12, 18, 10, 12, 24, 44, 15]);
    }

    #[test]
    fn conformance_fixture_020_valid_multipoint_geometry() {
        // mapbox/mvt-fixtures#020
        assert_matches_mvt_fixture("020", GeomType::Point, &[17, 10, 14, 3, 9]);
    }

    #[test]
    fn conformance_fixture_021_valid_multilinestring_geometry() {
        // mapbox/mvt-fixtures#021
        assert_matches_mvt_fixture(
            "021",
            GeomType::LineString,
            &[9, 4, 4, 18, 0, 16, 16, 0, 9, 17, 17, 10, 4, 8],
        );
    }

    #[test]
    fn conformance_fixture_022_valid_multipolygon_geometry() {
        // mapbox/mvt-fixtures#022
        assert_matches_mvt_fixture(
            "022",
            GeomType::Polygon,
            &[
                9, 0, 0, 26, 20, 0, 0, 20, 19, 0, 15, 9, 22, 2, 26, 18, 0, 0, 18, 17, 0, 15,
                9, 4, 13, 26, 0, 8, 8, 0, 0, 7, 15,
            ],
        );
    }

    // -----------------------------------------------------------------------
    // append_geometry tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_append_geometry_single_linestring() {
        // Encode a linestring: (10,20) -> (30,40)
        let mut src = Vec::new();
        encode_linestring(&mut src, &[(10, 20), (30, 40)]);
        let mut dest = Vec::new();
        let mut cx: i32 = 0;
        let mut cy: i32 = 0;
        append_geometry(&mut dest, &src, &mut cx, &mut cy);
        // First geometry appended with cursor at (0,0) should be identical to source
        assert_eq!(dest, src);
        assert_eq!(cx, 30);
        assert_eq!(cy, 40);
    }

    #[test]
    fn test_append_geometry_two_linestrings() {
        // First linestring: (10,20) -> (30,40)
        let mut src1 = Vec::new();
        encode_linestring(&mut src1, &[(10, 20), (30, 40)]);
        // Second linestring: (5,5) -> (15,15)
        let mut src2 = Vec::new();
        encode_linestring(&mut src2, &[(5, 5), (15, 15)]);

        let mut dest = Vec::new();
        let mut cx: i32 = 0;
        let mut cy: i32 = 0;
        append_geometry(&mut dest, &src1, &mut cx, &mut cy);
        assert_eq!(cx, 30);
        assert_eq!(cy, 40);
        append_geometry(&mut dest, &src2, &mut cx, &mut cy);
        assert_eq!(cx, 15);
        assert_eq!(cy, 15);

        // Decode the concatenated geometry back to absolute coordinates
        let coords = decode_commands_to_abs(&dest);
        // Should have: MoveTo(10,20), LineTo(30,40), MoveTo(5,5), LineTo(15,15)
        assert_eq!(coords, vec![(10, 20), (30, 40), (5, 5), (15, 15)]);
    }

    #[test]
    fn test_append_geometry_polygon_closepath() {
        // Triangle polygon: (0,0) -> (100,0) -> (50,100) -> close
        let ring = [(0, 0), (100, 0), (50, 100), (0, 0)];
        let mut src = Vec::new();
        encode_polygon(&mut src, &[&ring]);

        let mut dest = Vec::new();
        let mut cx: i32 = 0;
        let mut cy: i32 = 0;
        append_geometry(&mut dest, &src, &mut cx, &mut cy);
        // After ClosePath, cursor resets to the MoveTo position (0,0)
        assert_eq!(cx, 0);
        assert_eq!(cy, 0);
    }

    #[test]
    fn test_append_geometry_two_polygons_cursor_reset() {
        // Polygon 1: square at (0,0)
        let ring1 = [(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
        let mut src1 = Vec::new();
        encode_polygon(&mut src1, &[&ring1]);

        // Polygon 2: square at (100,100)
        let ring2 = [(100, 100), (110, 100), (110, 110), (100, 110), (100, 100)];
        let mut src2 = Vec::new();
        encode_polygon(&mut src2, &[&ring2]);

        let mut dest = Vec::new();
        let mut cx: i32 = 0;
        let mut cy: i32 = 0;
        append_geometry(&mut dest, &src1, &mut cx, &mut cy);
        // After close, cursor is at ring1's MoveTo = (0,0)
        assert_eq!(cx, 0);
        assert_eq!(cy, 0);
        append_geometry(&mut dest, &src2, &mut cx, &mut cy);
        // After close, cursor is at ring2's MoveTo = (100,100)
        assert_eq!(cx, 100);
        assert_eq!(cy, 100);

        // Decode and verify all absolute positions are correct
        let coords = decode_commands_to_abs(&dest);
        // ring1: MoveTo(0,0), LineTo(10,0), LineTo(10,10), LineTo(0,10)
        // ring2: MoveTo(100,100), LineTo(110,100), LineTo(110,110), LineTo(100,110)
        assert_eq!(coords[0], (0, 0));
        assert_eq!(coords[1], (10, 0));
        assert_eq!(coords[4], (100, 100));
        assert_eq!(coords[5], (110, 100));
    }

    /// Decode MVT commands into absolute (x,y) coordinates (ignoring ClosePath).
    fn decode_commands_to_abs(cmds: &[u32]) -> Vec<(i32, i32)> {
        let mut result = Vec::new();
        let mut cx: i32 = 0;
        let mut cy: i32 = 0;
        let mut last_move_x: i32 = 0;
        let mut last_move_y: i32 = 0;
        let mut i = 0;
        while i < cmds.len() {
            let cmd = cmds[i];
            let cmd_id = cmd & 0x7;
            let cmd_count = cmd >> 3;
            i += 1;
            match cmd_id {
                1 | 2 => {
                    for _ in 0..cmd_count {
                        let dx = decode_zigzag(cmds[i]);
                        let dy = decode_zigzag(cmds[i + 1]);
                        cx += dx;
                        cy += dy;
                        result.push((cx, cy));
                        if cmd_id == 1 {
                            last_move_x = cx;
                            last_move_y = cy;
                        }
                        i += 2;
                    }
                }
                7 => {
                    // ClosePath resets cursor to last MoveTo position
                    cx = last_move_x;
                    cy = last_move_y;
                }
                _ => {}
            }
        }
        result
    }

    // -----------------------------------------------------------------------
    // merge_same_attr_geometries tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_merge_no_features() {
        let mut layer = LayerBuilder::new("test");
        let mut scratch = MergeScratch::new();
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);
        assert_eq!(layer.test_feature_count(), 0);
    }

    #[test]
    fn test_merge_single_feature() {
        let mut layer = LayerBuilder::new("test");
        let ki = layer.intern_key("kind");
        let vi = layer.intern_value(Value::String("motorway".into()));
        let mut geom = Vec::new();
        encode_linestring(&mut geom, &[(0, 0), (10, 10)]);
        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::LineString,
            geometry: geom,
            tags: vec![(ki, vi)],
        });
        let mut scratch = MergeScratch::new();
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);
        assert_eq!(layer.test_feature_count(), 1);
    }

    #[test]
    fn test_merge_two_lines_same_attrs() {
        let mut layer = LayerBuilder::new("test");
        let ki = layer.intern_key("kind");
        let vi = layer.intern_value(Value::String("residential".into()));

        let mut geom1 = Vec::new();
        encode_linestring(&mut geom1, &[(0, 0), (10, 10)]);
        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::LineString,
            geometry: geom1,
            tags: vec![(ki, vi)],
        });

        let mut geom2 = Vec::new();
        encode_linestring(&mut geom2, &[(20, 20), (30, 30)]);
        layer.add_feature(Feature {
            id: Some(2),
            geom_type: GeomType::LineString,
            geometry: geom2,
            tags: vec![(ki, vi)],
        });

        let mut scratch = MergeScratch::new();
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

        // Should merge into 1 feature
        assert_eq!(layer.test_feature_count(), 1);
        // Merged feature should have no ID
        assert_eq!(layer.test_feature(0).id, None);
        // Geometry should contain both linestrings
        let coords = decode_commands_to_abs(&layer.test_feature(0).geometry);
        assert_eq!(coords.len(), 4); // 2 points from each linestring
        assert_eq!(coords[0], (0, 0));
        assert_eq!(coords[1], (10, 10));
        assert_eq!(coords[2], (20, 20));
        assert_eq!(coords[3], (30, 30));
    }

    #[test]
    fn test_merge_different_attrs_not_merged() {
        let mut layer = LayerBuilder::new("test");
        let ki = layer.intern_key("kind");
        let v1 = layer.intern_value(Value::String("residential".into()));
        let v2 = layer.intern_value(Value::String("motorway".into()));

        let mut geom1 = Vec::new();
        encode_linestring(&mut geom1, &[(0, 0), (10, 10)]);
        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::LineString,
            geometry: geom1,
            tags: vec![(ki, v1)],
        });

        let mut geom2 = Vec::new();
        encode_linestring(&mut geom2, &[(20, 20), (30, 30)]);
        layer.add_feature(Feature {
            id: Some(2),
            geom_type: GeomType::LineString,
            geometry: geom2,
            tags: vec![(ki, v2)],
        });

        let mut scratch = MergeScratch::new();
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

        // Different attrs → no merge
        assert_eq!(layer.test_feature_count(), 2);
    }

    #[test]
    fn test_merge_points_not_merged() {
        let mut layer = LayerBuilder::new("test");
        let ki = layer.intern_key("kind");
        let vi = layer.intern_value(Value::String("city".into()));

        let mut geom1 = Vec::new();
        encode_point(&mut geom1, 10, 20);
        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::Point,
            geometry: geom1,
            tags: vec![(ki, vi)],
        });

        let mut geom2 = Vec::new();
        encode_point(&mut geom2, 30, 40);
        layer.add_feature(Feature {
            id: Some(2),
            geom_type: GeomType::Point,
            geometry: geom2,
            tags: vec![(ki, vi)],
        });

        let mut scratch = MergeScratch::new();
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

        // Points are explicitly skipped
        assert_eq!(layer.test_feature_count(), 2);
    }

    #[test]
    fn test_merge_mixed_geom_types_separate() {
        let mut layer = LayerBuilder::new("test");
        let ki = layer.intern_key("kind");
        let vi = layer.intern_value(Value::String("residential".into()));

        // A linestring
        let mut geom1 = Vec::new();
        encode_linestring(&mut geom1, &[(0, 0), (10, 10)]);
        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::LineString,
            geometry: geom1,
            tags: vec![(ki, vi)],
        });

        // A polygon with same attrs
        let ring = [(0, 0), (10, 0), (10, 10), (0, 0)];
        let mut geom2 = Vec::new();
        encode_polygon(&mut geom2, &[&ring]);
        layer.add_feature(Feature {
            id: Some(2),
            geom_type: GeomType::Polygon,
            geometry: geom2,
            tags: vec![(ki, vi)],
        });

        let mut scratch = MergeScratch::new();
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

        // Different geom types → no merge (even with same tags)
        assert_eq!(layer.test_feature_count(), 2);
    }

    #[test]
    fn test_merge_three_lines_same_attrs() {
        let mut layer = LayerBuilder::new("test");
        let ki = layer.intern_key("kind");
        let vi = layer.intern_value(Value::String("path".into()));

        for i in 0..3 {
            let mut geom = Vec::new();
            let base = i * 100;
            encode_linestring(&mut geom, &[(base, base), (base + 10, base + 10)]);
            layer.add_feature(Feature {
                #[allow(clippy::cast_sign_loss)]
                id: Some(i as u64),
                geom_type: GeomType::LineString,
                geometry: geom,
                tags: vec![(ki, vi)],
            });
        }

        let mut scratch = MergeScratch::new();
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

        // 3 features → 1 merged feature
        assert_eq!(layer.test_feature_count(), 1);
        let coords = decode_commands_to_abs(&layer.test_feature(0).geometry);
        assert_eq!(coords.len(), 6); // 2 points × 3 linestrings
    }

    #[test]
    fn test_merge_reclaims_to_pools() {
        let mut layer = LayerBuilder::new("test");
        let ki = layer.intern_key("kind");
        let vi = layer.intern_value(Value::String("residential".into()));

        for i in 0..3 {
            let mut geom = Vec::new();
            encode_linestring(&mut geom, &[(i * 10, 0), (i * 10 + 5, 5)]);
            layer.add_feature(Feature {
                #[allow(clippy::cast_sign_loss)]
                id: Some(i as u64),
                geom_type: GeomType::LineString,
                geometry: geom,
                tags: vec![(ki, vi)],
            });
        }

        let mut scratch = MergeScratch::new();
        let mut gp: Vec<Vec<u32>> = Vec::new();
        let mut tp: Vec<Vec<(u16, u16)>> = Vec::new();
        layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

        // Secondary features' vecs should be reclaimed into pools
        assert_eq!(gp.len(), 2); // 2 secondaries reclaimed
        assert_eq!(tp.len(), 2);
    }
}
