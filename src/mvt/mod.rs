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

mod merge;

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

/// Sort key for Value: groups by type discriminant, then by canonical value.
/// Clusters same-type values together in the protobuf value table so gzip
/// sees longer runs of similar byte patterns.
fn value_sort_key(v: &Value) -> (u8, u64, &str) {
    match v {
        Value::String(s) => (0, 0, s.as_str()),
        Value::Float(f) => (1, f.to_bits() as u64, ""),
        Value::Double(d) => (2, d.to_bits(), ""),
        #[allow(clippy::cast_sign_loss)]
        Value::Int(i) => (3, *i as u64, ""),
        Value::UInt(u) => (4, *u, ""),
        #[allow(clippy::cast_sign_loss)]
        Value::SInt(i) => (5, *i as u64, ""),
        Value::Bool(b) => (6, u64::from(*b), ""),
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
    /// Sorted key permutation: sorted_keys[new_idx] = old_idx.
    sorted_keys: Vec<u16>,
    /// Inverse key map: key_remap[old_idx] = new_idx.
    key_remap: Vec<u16>,
    /// Sorted value permutation: sorted_vals[new_idx] = old_idx.
    sorted_vals: Vec<u16>,
    /// Inverse value map: val_remap[old_idx] = new_idx.
    val_remap: Vec<u16>,
}

impl EncodeScratch {
    pub fn new() -> Self {
        Self {
            layer_buf: Vec::new(),
            feat_buf: Vec::new(),
            val_buf: Vec::new(),
            packed: Vec::new(),
            tag_vals: Vec::new(),
            sorted_keys: Vec::new(),
            key_remap: Vec::new(),
            sorted_vals: Vec::new(),
            val_remap: Vec::new(),
        }
    }
}

/// Reusable scratch buffers for geometry merging, avoiding per-tile allocation.
/// Created once per thread via `thread_local!`, reused across all tiles on that worker.
pub struct MergeScratch {
    pub(super) indices: Vec<usize>,
    pub(super) geom: Vec<u32>,
}

impl MergeScratch {
    pub fn new() -> Self {
        Self {
            indices: Vec::new(),
            geom: Vec::new(),
        }
    }
}

/// Reusable scratch buffers for line merging.
pub struct LineMergeScratch {
    pub(super) segments: Vec<Vec<(i32, i32)>>,
    pub(super) merged: Vec<Vec<(i32, i32)>>,
    pub(super) visited: Vec<bool>,
    pub(super) starts: Vec<(i32, i32, usize, bool)>,
    pub(super) chain: Vec<(i32, i32)>,
    pub(super) encode_buf: Vec<u32>,
}

impl LineMergeScratch {
    pub fn new() -> Self {
        Self {
            segments: Vec::new(),
            merged: Vec::new(),
            visited: Vec::new(),
            starts: Vec::new(),
            chain: Vec::new(),
            encode_buf: Vec::new(),
        }
    }
}

pub struct LayerBuilder {
    name: String,
    pub(super) features: Vec<Feature>,
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

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn features(&self) -> &[Feature] {
        &self.features
    }

    pub(crate) fn features_mut(&mut self) -> &mut [Feature] {
        &mut self.features
    }

    pub(crate) fn key(&self, idx: u16) -> Option<&str> {
        self.keys.get(idx as usize).map(String::as_str)
    }

    pub(crate) fn value(&self, idx: u16) -> Option<&Value> {
        self.values.get(idx as usize)
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

    #[allow(clippy::cast_possible_truncation)]
    fn encode(&self, buf: &mut Vec<u8>, s: &mut EncodeScratch) {
        s.layer_buf.clear();

        // Build sorted key permutation (alphabetical) and inverse remap.
        s.sorted_keys.clear();
        s.sorted_keys.extend(0..self.keys.len() as u16);
        s.sorted_keys.sort_by(|&a, &b| self.keys[a as usize].cmp(&self.keys[b as usize]));
        s.key_remap.clear();
        s.key_remap.resize(self.keys.len(), 0);
        for (new_idx, &old_idx) in s.sorted_keys.iter().enumerate() {
            s.key_remap[old_idx as usize] = new_idx as u16;
        }

        // Build sorted value permutation (by type then canonical value) and inverse remap.
        s.sorted_vals.clear();
        s.sorted_vals.extend(0..self.values.len() as u16);
        s.sorted_vals.sort_by(|&a, &b| {
            value_sort_key(&self.values[a as usize]).cmp(&value_sort_key(&self.values[b as usize]))
        });
        s.val_remap.clear();
        s.val_remap.resize(self.values.len(), 0);
        for (new_idx, &old_idx) in s.sorted_vals.iter().enumerate() {
            s.val_remap[old_idx as usize] = new_idx as u16;
        }

        // field 15: version = 2
        encode_varint_field_always(&mut s.layer_buf, 15, 2);
        // field 1: name
        encode_bytes_field_always(&mut s.layer_buf, 1, self.name.as_bytes());
        // field 5: extent = 4096
        encode_varint_field_always(&mut s.layer_buf, 5, 4096);

        // field 2: features (tag indices remapped to sorted positions)
        for f in &self.features {
            s.feat_buf.clear();
            if let Some(id) = f.id {
                encode_varint_field_always(&mut s.feat_buf, 1, id);
            }
            if !f.tags.is_empty() {
                s.tag_vals.clear();
                s.tag_vals.extend(f.tags.iter().flat_map(|&(k, v)| {
                    [u32::from(s.key_remap[k as usize]), u32::from(s.val_remap[v as usize])]
                }));
                encode_packed_uint32(&mut s.feat_buf, &mut s.packed, 2, &s.tag_vals);
            }
            encode_varint_field_always(&mut s.feat_buf, 3, f.geom_type as u64);
            if !f.geometry.is_empty() {
                encode_packed_uint32(&mut s.feat_buf, &mut s.packed, 4, &f.geometry);
            }
            encode_bytes_field_always(&mut s.layer_buf, 2, &s.feat_buf);
        }

        // field 3: keys (sorted alphabetically)
        for &old_idx in &s.sorted_keys {
            encode_bytes_field_always(&mut s.layer_buf, 3, self.keys[old_idx as usize].as_bytes());
        }

        // field 4: values (sorted by type then value)
        for &old_idx in &s.sorted_vals {
            s.val_buf.clear();
            encode_value(&mut s.val_buf, &self.values[old_idx as usize]);
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
pub(super) fn zigzag(v: i32) -> u32 {
    #[allow(clippy::cast_sign_loss)]
    { ((v << 1) ^ (v >> 31)) as u32 }
}

#[inline]
pub(super) fn command(id: u32, count: u32) -> u32 {
    id | (count << 3)
}

#[inline]
pub(super) fn decode_zigzag(v: u32) -> i32 {
    #[allow(clippy::cast_possible_wrap)]
    { ((v >> 1) as i32) ^ (-((v & 1) as i32)) }
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
#[path = "tests.rs"]
mod tests;
