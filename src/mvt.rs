// MVT (Mapbox Vector Tile) protobuf encoder.
//
// Hand-rolled protobuf encoding — the MVT schema is simple enough that codegen
// is unnecessary. Produces spec-compliant tiles with extent=4096.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum GeomType {
    Point = 1,
    LineString = 2,
    Polygon = 3,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    String(String),
    Float(f32),
    Double(f64),
    Int(i64),
    UInt(u64),
    SInt(i64),
    Bool(bool),
}

pub struct Feature {
    pub id: Option<u64>,
    pub geom_type: GeomType,
    pub geometry: Vec<u32>,
    pub tags: Vec<(u16, u16)>,
}

pub struct LayerBuilder {
    name: String,
    features: Vec<Feature>,
    keys: Vec<String>,
    key_map: HashMap<String, u16>,
    values: Vec<Value>,
    value_map: HashMap<u64, u16>,
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
            key_map: HashMap::new(),
            values: Vec::new(),
            value_map: HashMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }

    /// Reset for reuse without deallocating backing memory.
    pub fn clear(&mut self) {
        self.features.clear();
        self.keys.clear();
        self.key_map.clear();
        self.values.clear();
        self.value_map.clear();
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn intern_key(&mut self, key: &str) -> u16 {
        if let Some(&idx) = self.key_map.get(key) {
            return idx;
        }
        let idx = self.keys.len() as u16;
        self.keys.push(key.to_string());
        self.key_map.insert(key.to_string(), idx);
        idx
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn intern_value(&mut self, val: Value) -> u16 {
        let hash = value_hash(&val);
        if let Some(&idx) = self.value_map.get(&hash)
            && self.values[idx as usize] == val
        {
            return idx;
        }
        let idx = self.values.len() as u16;
        self.values.push(val);
        self.value_map.insert(hash, idx);
        idx
    }

    pub fn add_feature(&mut self, feature: Feature) {
        self.features.push(feature);
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        let mut layer_buf = Vec::new();

        // field 15: version = 2
        encode_field_varint(&mut layer_buf, 15, 2);
        // field 1: name
        encode_field_bytes(&mut layer_buf, 1, self.name.as_bytes());
        // field 5: extent = 4096
        encode_field_varint(&mut layer_buf, 5, 4096);

        // field 2: features
        for f in &self.features {
            let mut feat_buf = Vec::new();
            if let Some(id) = f.id {
                encode_field_varint(&mut feat_buf, 1, id);
            }
            if !f.tags.is_empty() {
                let tag_vals: Vec<u32> = f
                    .tags
                    .iter()
                    .flat_map(|&(k, v)| [u32::from(k), u32::from(v)])
                    .collect();
                encode_packed_u32(&mut feat_buf, 2, &tag_vals);
            }
            encode_field_varint(&mut feat_buf, 3, f.geom_type as u64);
            if !f.geometry.is_empty() {
                encode_packed_u32(&mut feat_buf, 4, &f.geometry);
            }
            encode_field_bytes(&mut layer_buf, 2, &feat_buf);
        }

        // field 3: keys
        for k in &self.keys {
            encode_field_bytes(&mut layer_buf, 3, k.as_bytes());
        }

        // field 4: values
        for v in &self.values {
            let mut val_buf = Vec::new();
            encode_value(&mut val_buf, v);
            encode_field_bytes(&mut layer_buf, 4, &val_buf);
        }

        // Write as field 3 (Tile.layers) length-delimited
        encode_field_bytes(buf, 3, &layer_buf);
    }
}

// ---------------------------------------------------------------------------
// Top-level encoder
// ---------------------------------------------------------------------------

pub fn encode_tile(layers: &[&LayerBuilder]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4096);
    for layer in layers {
        if !layer.is_empty() {
            layer.encode(&mut buf);
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

pub fn encode_multi_point(points: &[(i32, i32)]) -> Vec<u32> {
    if points.is_empty() {
        return Vec::new();
    }
    let mut cmds = Vec::with_capacity(1 + points.len() * 2);
    #[allow(clippy::cast_possible_truncation)]
    cmds.push(command(1, points.len() as u32));
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    for &(x, y) in points {
        cmds.push(zigzag(x - cx));
        cmds.push(zigzag(y - cy));
        cx = x;
        cy = y;
    }
    cmds
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
    // LineTo remaining points
    #[allow(clippy::cast_possible_truncation)]
    buf.push(command(2, (coords.len() - 1) as u32));
    let mut cx = coords[0].0;
    let mut cy = coords[0].1;
    for &(x, y) in &coords[1..] {
        buf.push(zigzag(x - cx));
        buf.push(zigzag(y - cy));
        cx = x;
        cy = y;
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
        // MoveTo first point
        buf.push(command(1, 1));
        buf.push(zigzag(points[0].0 - cx));
        buf.push(zigzag(points[0].1 - cy));
        cx = points[0].0;
        cy = points[0].1;
        // LineTo remaining (excluding last which is same as first)
        if points.len() > 1 {
            #[allow(clippy::cast_possible_truncation)]
            buf.push(command(2, (points.len() - 1) as u32));
            for &(x, y) in &points[1..] {
                buf.push(zigzag(x - cx));
                buf.push(zigzag(y - cy));
                cx = x;
                cy = y;
            }
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

fn encode_packed_u32(buf: &mut Vec<u8>, field: u32, vals: &[u32]) {
    let mut packed = Vec::new();
    for &v in vals {
        encode_varint(&mut packed, u64::from(v));
    }
    encode_field_bytes(buf, field, &packed);
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

fn value_hash(val: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(val).hash(&mut hasher);
    match val {
        Value::String(s) => s.hash(&mut hasher),
        Value::Float(f) => f.to_bits().hash(&mut hasher),
        Value::Double(d) => d.to_bits().hash(&mut hasher),
        Value::Int(i) | Value::SInt(i) => i.hash(&mut hasher),
        Value::UInt(u) => u.hash(&mut hasher),
        Value::Bool(b) => b.hash(&mut hasher),
    }
    hasher.finish()
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
