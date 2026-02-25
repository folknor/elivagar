// MVT (Mapbox Vector Tile) protobuf encoder.
//
// Hand-rolled protobuf encoding — the MVT schema is simple enough that codegen
// is unnecessary. Produces spec-compliant tiles with extent=4096.

// FxHashMap (rustc-hash): non-cryptographic hash ~3× faster than std SipHash for
// small keys. Safe here because keys are short strings and interned integers — no
// adversarial input. Tradeoff: weaker collision resistance (irrelevant for tile
// encoding). Already a transitive dependency via roaring. To revert, swap back to
// std::collections::HashMap and remove the rustc-hash direct dependency.
use rustc_hash::FxHashMap;
use std::hash::{Hash, Hasher};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

/// Reusable scratch buffers for geometry merging, avoiding per-tile HashMap allocation.
/// Created once per rayon worker via `map_init`, reused across all tiles on that worker.
pub struct MergeScratch {
    groups: FxHashMap<(GeomType, Vec<(u16, u16)>), Vec<usize>>,
    geom: Vec<u32>,
}

impl MergeScratch {
    pub fn new() -> Self {
        Self {
            groups: FxHashMap::default(),
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
        let idx = self.keys.len() as u16;
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
        let idx = self.values.len() as u16;
        self.value_map.insert(val.clone(), idx);
        self.values.push(val);
        idx
    }

    /// Intern a string value by borrowed `&str`, avoiding allocation on cache hit.
    /// Same pattern as `intern_key`: `HashMap<String, u16>` supports `get(&str)`
    /// because `String: Borrow<str>`.
    #[allow(clippy::cast_possible_truncation)]
    pub fn intern_string_value(&mut self, s: &str) -> u16 {
        if let Some(&idx) = self.string_value_map.get(s) {
            return idx;
        }
        let idx = self.values.len() as u16;
        let owned = s.to_string();
        self.values.push(Value::String(owned.clone()));
        self.string_value_map.insert(owned, idx);
        idx
    }

    pub fn add_feature(&mut self, feature: Feature) {
        self.features.push(feature);
    }

    /// Drain all features and push their geometry/tags Vecs into pools for reuse.
    /// Called after `encode_tile_with` to recover allocated buffers.
    pub fn reclaim_features(
        &mut self,
        geom_pool: &mut Vec<Vec<u32>>,
        tags_pool: &mut Vec<Vec<(u16, u16)>>,
    ) {
        for f in self.features.drain(..) {
            geom_pool.push(f.geometry);
            tags_pool.push(f.tags);
        }
    }

    fn encode(&self, buf: &mut Vec<u8>, s: &mut EncodeScratch) {
        s.layer_buf.clear();

        // field 15: version = 2
        encode_field_varint(&mut s.layer_buf, 15, 2);
        // field 1: name
        encode_field_bytes(&mut s.layer_buf, 1, self.name.as_bytes());
        // field 5: extent = 4096
        encode_field_varint(&mut s.layer_buf, 5, 4096);

        // field 2: features
        for f in &self.features {
            s.feat_buf.clear();
            if let Some(id) = f.id {
                encode_field_varint(&mut s.feat_buf, 1, id);
            }
            if !f.tags.is_empty() {
                s.tag_vals.clear();
                s.tag_vals.extend(
                    f.tags
                        .iter()
                        .flat_map(|&(k, v)| [u32::from(k), u32::from(v)]),
                );
                encode_packed_u32(&mut s.feat_buf, 2, &s.tag_vals, &mut s.packed);
            }
            encode_field_varint(&mut s.feat_buf, 3, f.geom_type as u64);
            if !f.geometry.is_empty() {
                encode_packed_u32(&mut s.feat_buf, 4, &f.geometry, &mut s.packed);
            }
            encode_field_bytes(&mut s.layer_buf, 2, &s.feat_buf);
        }

        // field 3: keys
        for k in &self.keys {
            encode_field_bytes(&mut s.layer_buf, 3, k.as_bytes());
        }

        // field 4: values
        for v in &self.values {
            s.val_buf.clear();
            encode_value(&mut s.val_buf, v);
            encode_field_bytes(&mut s.layer_buf, 4, &s.val_buf);
        }

        // Write as field 3 (Tile.layers) length-delimited
        encode_field_bytes(buf, 3, &s.layer_buf);
    }
}

// ---------------------------------------------------------------------------
// Top-level encoder
// ---------------------------------------------------------------------------

#[cfg(test)]
pub fn encode_tile(layers: &[&LayerBuilder]) -> Vec<u8> {
    let mut scratch = EncodeScratch::new();
    encode_tile_with(layers, &mut scratch)
}

#[hotpath::measure]
pub fn encode_tile_with(layers: &[&LayerBuilder], scratch: &mut EncodeScratch) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4096);
    for layer in layers {
        if !layer.is_empty() {
            layer.encode(&mut buf, scratch);
        }
    }
    buf
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

// ---------------------------------------------------------------------------
// Low-level protobuf encoding
// ---------------------------------------------------------------------------

fn encode_varint(buf: &mut Vec<u8>, mut val: u64) {
    loop {
        #[allow(clippy::cast_possible_truncation)]
        if val < 0x80 {
            buf.push(val as u8);
            break;
        }
        #[allow(clippy::cast_possible_truncation)]
        buf.push((val as u8 & 0x7F) | 0x80);
        val >>= 7;
    }
}

fn encode_field_varint(buf: &mut Vec<u8>, field: u32, val: u64) {
    encode_varint(buf, u64::from(field << 3)); // wire type 0
    encode_varint(buf, val);
}

fn encode_field_bytes(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    encode_varint(buf, u64::from(field << 3 | 2)); // wire type 2
    #[allow(clippy::cast_possible_truncation)]
    encode_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

fn encode_packed_u32(buf: &mut Vec<u8>, field: u32, vals: &[u32], packed: &mut Vec<u8>) {
    packed.clear();
    for &v in vals {
        encode_varint(packed, u64::from(v));
    }
    encode_field_bytes(buf, field, packed);
}

fn encode_value(buf: &mut Vec<u8>, val: &Value) {
    match val {
        Value::String(s) => encode_field_bytes(buf, 1, s.as_bytes()),
        Value::Float(f) => {
            encode_varint(buf, u64::from(2u32 << 3 | 5)); // field 2, wire type 5 (32-bit)
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::Double(d) => {
            encode_varint(buf, u64::from(3u32 << 3 | 1)); // field 3, wire type 1 (64-bit)
            buf.extend_from_slice(&d.to_le_bytes());
        }
        #[allow(clippy::cast_sign_loss)]
        Value::Int(i) => encode_field_varint(buf, 4, *i as u64),
        Value::UInt(u) => encode_field_varint(buf, 5, *u),
        Value::SInt(i) => {
            encode_varint(buf, u64::from(6u32 << 3)); // field 6, wire type 0
            encode_varint(buf, zigzag_i64(*i));
        }
        Value::Bool(b) => encode_field_varint(buf, 7, u64::from(*b)),
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
fn zigzag_i64(v: i64) -> u64 {
    #[allow(clippy::cast_sign_loss)]
    { ((v << 1) ^ (v >> 63)) as u64 }
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
                // Known: cmd is pushed before validating that src has enough
                // parameters. If src were truncated, the command count would
                // be wrong. Not reachable: geometry is always well-formed from
                // the closed encode/serialize/deserialize pipeline.
                dest.push(cmd);
                for _ in 0..cmd_count {
                    if i + 1 >= src.len() {
                        break;
                    }
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
    /// Uses a reusable `MergeScratch` (hoisted HashMap + geometry buffer) to
    /// avoid per-tile allocation. Tags are cloned for HashMap keys but not
    /// sorted — shortbread matching produces tags in deterministic order.
    /// Merges in-place: appends secondary geometries into the first feature
    /// of each group via scratch buffer + swap, tombstones secondaries with
    /// empty geometry, then retains non-tombstone features.
    #[hotpath::measure]
    pub fn merge_same_attr_geometries(
        &mut self,
        scratch: &mut MergeScratch,
        geom_pool: &mut Vec<Vec<u32>>,
        tags_pool: &mut Vec<Vec<(u16, u16)>>,
    ) {
        if self.features.len() < 2 {
            return;
        }

        // Group features by (geom_type, tags). Tags are deterministic from
        // shortbread matching — no sort needed. HashMap is reused across tiles
        // (`.clear()` retains allocated capacity).
        scratch.groups.clear();
        for (i, f) in self.features.iter().enumerate() {
            if f.geom_type == GeomType::Point {
                continue;
            }
            scratch.groups
                .entry((f.geom_type, f.tags.clone()))
                .or_default()
                .push(i);
        }

        // Check if any group has >1 feature worth merging
        let any_mergeable = scratch.groups.values().any(|v| v.len() > 1);
        if !any_mergeable {
            return;
        }

        // In-place merge: for each group, concatenate all geometries into
        // scratch.geom, swap into first feature, reclaim secondaries' Vecs.
        for (_, indices) in &scratch.groups {
            if indices.len() < 2 {
                continue;
            }
            // Concatenate all geometries into scratch buffer
            scratch.geom.clear();
            let mut cx: i32 = 0;
            let mut cy: i32 = 0;
            for &idx in indices {
                append_geometry(&mut scratch.geom, &self.features[idx].geometry, &mut cx, &mut cy);
            }
            // Swap merged geometry into first feature
            let first = indices[0];
            std::mem::swap(&mut self.features[first].geometry, &mut scratch.geom);
            self.features[first].id = None;
            // Reclaim secondary features' Vecs into pools (mem::take leaves
            // zero-capacity Vecs so retain can identify dead features).
            for &idx in &indices[1..] {
                geom_pool.push(std::mem::take(&mut self.features[idx].geometry));
                tags_pool.push(std::mem::take(&mut self.features[idx].tags));
            }
        }

        // Remove dead features (zero-capacity Vecs from mem::take).
        // Uses is_empty() as tombstone proxy — safe because all pipeline-emitted
        // features have non-empty geometry (enforced at all emit call sites).
        self.features.retain(|f| !f.geometry.is_empty());
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
}
