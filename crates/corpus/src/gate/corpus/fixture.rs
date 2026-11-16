//! Test-only MVT and PMTiles fixture construction, shared by the canonical-hash
//! equivalence tests and the regress engine tests.
//!
//! It has to be an encoder rather than committed golden bytes for two reasons.
//! Several fixtures are deliberately hostile - unknown fields at every message
//! level, packed fields split across repeated occurrences, a layer with no
//! version - and a production encoder is designed never to emit those. And the
//! properties under test are relational ("these two tiles differ only in feature
//! order"), which needs the two tiles generated from one description rather than
//! two opaque blobs asserted to be related.
//!
//! Faithful to `elivagar::mvt`'s encoding conventions - keys sorted
//! alphabetically, values sorted by (type, canonical value), tag indices
//! remapped to the sorted positions - so the fixtures exercise the same wire
//! shapes the pipeline produces. The tag/geometry framing is MVT-spec truth and
//! self-checks against the real decoder.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::super::constants::mvt;
use super::super::eliv::{
    LonLatBbox, MapCenter, PmtilesConfig, PmtilesWriter, WIRE_32BIT, WIRE_64BIT,
    encode_bytes_field_always, encode_tag, encode_varint, encode_varint_field_always,
    xy_to_tile_id,
};
use flate2::Compression;
use flate2::write::GzEncoder;

// ---------------------------------------------------------------------------
// MVT encoder
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum GeomType {
    Point = mvt::GEOMETRY_POINT,
    LineString = mvt::GEOMETRY_LINESTRING,
    Polygon = mvt::GEOMETRY_POLYGON,
}

#[derive(Clone)]
pub(crate) enum Value {
    String(String),
    Float(f32),
    Double(f64),
}

fn value_sort_key(v: &Value) -> (u8, u64, String) {
    match v {
        Value::String(s) => (0, 0, s.clone()),
        Value::Float(f) => (1, u64::from(f.to_bits()), String::new()),
        Value::Double(d) => (2, d.to_bits(), String::new()),
    }
}

/// Bit equality, not numeric: the float fixtures exist to pin that two distinct
/// NaN payloads stay distinct through the hash, which `==` would erase.
fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        _ => false,
    }
}

pub(crate) struct Feature {
    pub(crate) id: Option<u64>,
    pub(crate) geom_type: GeomType,
    pub(crate) geometry: Vec<u32>,
    pub(crate) tags: Vec<(u16, u16)>,
}

pub(crate) struct LayerBuilder {
    name: String,
    features: Vec<Feature>,
    keys: Vec<String>,
    values: Vec<Value>,
}

#[allow(clippy::cast_possible_truncation)]
impl LayerBuilder {
    pub(crate) fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            features: Vec::new(),
            keys: Vec::new(),
            values: Vec::new(),
        }
    }
    pub(crate) fn intern_key(&mut self, key: &str) -> u16 {
        if let Some(i) = self.keys.iter().position(|k| k == key) {
            return i as u16;
        }
        self.keys.push(key.to_string());
        (self.keys.len() - 1) as u16
    }
    pub(crate) fn intern_value(&mut self, val: Value) -> u16 {
        if let Some(i) = self.values.iter().position(|v| value_eq(v, &val)) {
            return i as u16;
        }
        self.values.push(val);
        (self.values.len() - 1) as u16
    }
    pub(crate) fn add_feature(&mut self, feature: Feature) {
        self.features.push(feature);
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        let mut layer = Vec::new();
        let mut sorted_keys: Vec<u16> = (0..self.keys.len() as u16).collect();
        sorted_keys.sort_by(|&a, &b| self.keys[a as usize].cmp(&self.keys[b as usize]));
        let mut key_remap = vec![0u16; self.keys.len()];
        for (new_idx, &old) in sorted_keys.iter().enumerate() {
            key_remap[old as usize] = new_idx as u16;
        }
        let mut sorted_vals: Vec<u16> = (0..self.values.len() as u16).collect();
        sorted_vals.sort_by(|&a, &b| {
            value_sort_key(&self.values[a as usize]).cmp(&value_sort_key(&self.values[b as usize]))
        });
        let mut val_remap = vec![0u16; self.values.len()];
        for (new_idx, &old) in sorted_vals.iter().enumerate() {
            val_remap[old as usize] = new_idx as u16;
        }

        encode_bytes_field_always(&mut layer, mvt::layer::NAME, self.name.as_bytes());
        for f in &self.features {
            let mut feat = Vec::new();
            if let Some(id) = f.id {
                encode_varint_field_always(&mut feat, mvt::feature::ID, id);
            }
            if !f.tags.is_empty() {
                let mut packed = Vec::new();
                for &(k, v) in &f.tags {
                    encode_varint(&mut packed, u64::from(key_remap[k as usize]));
                    encode_varint(&mut packed, u64::from(val_remap[v as usize]));
                }
                encode_bytes_field_always(&mut feat, mvt::feature::TAGS, &packed);
            }
            encode_varint_field_always(&mut feat, mvt::feature::GEOMETRY_TYPE, f.geom_type as u64);
            if !f.geometry.is_empty() {
                let mut packed = Vec::new();
                for &g in &f.geometry {
                    encode_varint(&mut packed, u64::from(g));
                }
                encode_bytes_field_always(&mut feat, mvt::feature::GEOMETRY, &packed);
            }
            encode_bytes_field_always(&mut layer, mvt::layer::FEATURE, &feat);
        }
        for &old in &sorted_keys {
            encode_bytes_field_always(
                &mut layer,
                mvt::layer::KEY,
                self.keys[old as usize].as_bytes(),
            );
        }
        for &old in &sorted_vals {
            let mut val = Vec::new();
            encode_value(&mut val, &self.values[old as usize]);
            encode_bytes_field_always(&mut layer, mvt::layer::VALUE, &val);
        }
        encode_varint_field_always(&mut layer, mvt::layer::EXTENT, u64::from(mvt::TILE_EXTENT));
        encode_varint_field_always(
            &mut layer,
            mvt::layer::VERSION,
            u64::from(mvt::FIXTURE_LAYER_VERSION),
        );
        encode_bytes_field_always(buf, mvt::TILE_LAYER, &layer);
    }
}

fn encode_value(buf: &mut Vec<u8>, val: &Value) {
    match val {
        Value::String(s) => encode_bytes_field_always(buf, mvt::value::STRING, s.as_bytes()),
        Value::Float(f) => {
            encode_tag(buf, mvt::value::FLOAT, WIRE_32BIT);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::Double(d) => {
            encode_tag(buf, mvt::value::DOUBLE, WIRE_64BIT);
            buf.extend_from_slice(&d.to_le_bytes());
        }
    }
}

/// Encode a tile from its layers. A featureless layer is dropped, matching the
/// pipeline - the fixtures that need an empty layer on the wire build it by
/// hand (`empty_layer_tile`).
pub(crate) fn encode_tile(layers: &[&LayerBuilder]) -> Vec<u8> {
    let mut buf = Vec::new();
    for layer in layers {
        if !layer.features.is_empty() {
            layer.encode(&mut buf);
        }
    }
    buf
}

// ---------------------------------------------------------------------------
// Geometry command encoding
// ---------------------------------------------------------------------------

pub(crate) fn command(id: u32, count: u32) -> u32 {
    id | (count << mvt::COMMAND_COUNT_SHIFT)
}

#[allow(clippy::cast_sign_loss)]
pub(crate) fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

pub(crate) fn encode_linestring(buf: &mut Vec<u32>, coords: &[(i32, i32)]) {
    buf.clear();
    if coords.len() < 2 {
        return;
    }
    buf.push(command(mvt::COMMAND_MOVE_TO, 1));
    buf.push(zigzag(coords[0].0));
    buf.push(zigzag(coords[0].1));
    let lineto = buf.len();
    buf.push(0);
    let (mut cx, mut cy) = (coords[0].0, coords[0].1);
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
        buf.clear();
        return;
    }
    buf[lineto] = command(mvt::COMMAND_LINE_TO, count);
}

pub(crate) fn encode_polygon(buf: &mut Vec<u32>, rings: &[&[(i32, i32)]]) {
    buf.clear();
    let (mut cx, mut cy) = (0i32, 0i32);
    for ring in rings {
        encode_polygon_ring(buf, ring, &mut cx, &mut cy);
    }
}

/// One ring, dropping the repeated closing vertex (ClosePath replaces it) and
/// leaving the cursor where the ring ended - ClosePath does NOT move the delta
/// cursor (MVT spec 4.3.3.3).
fn encode_polygon_ring(buf: &mut Vec<u32>, ring: &[(i32, i32)], cx: &mut i32, cy: &mut i32) {
    if ring.len() < 4 {
        return;
    }
    let points = &ring[..ring.len() - 1];
    let (save_len, save_cx, save_cy) = (buf.len(), *cx, *cy);
    buf.push(command(mvt::COMMAND_MOVE_TO, 1));
    buf.push(zigzag(points[0].0 - *cx));
    buf.push(zigzag(points[0].1 - *cy));
    *cx = points[0].0;
    *cy = points[0].1;
    let lineto = buf.len();
    buf.push(0);
    let mut count = 0u32;
    for &(x, y) in &points[1..] {
        if x == *cx && y == *cy {
            continue;
        }
        buf.push(zigzag(x - *cx));
        buf.push(zigzag(y - *cy));
        *cx = x;
        *cy = y;
        count += 1;
    }
    if count < 2 {
        buf.truncate(save_len);
        *cx = save_cx;
        *cy = save_cy;
        return;
    }
    buf[lineto] = command(mvt::COMMAND_LINE_TO, count);
    buf.push(command(mvt::COMMAND_CLOSE_PATH, 1));
}

pub(crate) fn encode_multiline(buf: &mut Vec<u32>, paths: &[&[(i32, i32)]]) {
    buf.clear();
    let (mut cx, mut cy) = (0i32, 0i32);
    for path in paths {
        if path.len() < 2 {
            continue;
        }
        buf.push(command(mvt::COMMAND_MOVE_TO, 1));
        buf.push(zigzag(path[0].0 - cx));
        buf.push(zigzag(path[0].1 - cy));
        cx = path[0].0;
        cy = path[0].1;
        let line_count = u32::try_from(path.len() - 1).expect("path length fits u32");
        buf.push(command(mvt::COMMAND_LINE_TO, line_count));
        for &(x, y) in &path[1..] {
            buf.push(zigzag(x - cx));
            buf.push(zigzag(y - cy));
            cx = x;
            cy = y;
        }
    }
}

// ---------------------------------------------------------------------------
// Tile fixtures
// ---------------------------------------------------------------------------

pub(crate) fn line_tile(id: Option<u64>, attr: &str, coords: &[(i32, i32)]) -> Vec<u8> {
    let mut layer = LayerBuilder::new("roads");
    let key = layer.intern_key("class");
    let val = layer.intern_value(Value::String(attr.to_string()));
    let mut geom = Vec::new();
    encode_linestring(&mut geom, coords);
    layer.add_feature(Feature {
        id,
        geom_type: GeomType::LineString,
        geometry: geom,
        tags: vec![(key, val)],
    });
    encode_tile(&[&layer])
}

pub(crate) fn float_attr_tile(value: Value) -> Vec<u8> {
    let mut layer = LayerBuilder::new("attrs");
    let key = layer.intern_key("v");
    let val = layer.intern_value(value);
    let mut geom = Vec::new();
    encode_linestring(&mut geom, &[(0, 0), (10, 10)]);
    layer.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::LineString,
        geometry: geom,
        tags: vec![(key, val)],
    });
    encode_tile(&[&layer])
}

pub(crate) fn multiline_tile(paths: &[&[(i32, i32)]]) -> Vec<u8> {
    let mut layer = LayerBuilder::new("roads");
    let key = layer.intern_key("class");
    let val = layer.intern_value(Value::String("path".to_string()));
    let mut geom = Vec::new();
    encode_multiline(&mut geom, paths);
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::LineString,
        geometry: geom,
        tags: vec![(key, val)],
    });
    encode_tile(&[&layer])
}

pub(crate) fn polygon_tile(rings: &[&[(i32, i32)]]) -> Vec<u8> {
    let mut layer = LayerBuilder::new("land");
    let mut geom = Vec::new();
    encode_polygon(&mut geom, rings);
    layer.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::Polygon,
        geometry: geom,
        tags: Vec::new(),
    });
    encode_tile(&[&layer])
}

pub(crate) fn point_tile(points: &[(i32, i32)]) -> Vec<u8> {
    let mut geometry = vec![command(
        mvt::COMMAND_MOVE_TO,
        u32::try_from(points.len()).expect("point count fits u32"),
    )];
    let (mut cx, mut cy) = (0, 0);
    for &(x, y) in points {
        geometry.push(zigzag(x - cx));
        geometry.push(zigzag(y - cy));
        cx = x;
        cy = y;
    }
    raw_geometry_tile(GeomType::Point, geometry)
}

pub(crate) fn raw_geometry_tile(geom_type: GeomType, geometry: Vec<u32>) -> Vec<u8> {
    let mut layer = LayerBuilder::new("raw");
    layer.add_feature(Feature {
        id: Some(1),
        geom_type,
        geometry,
        tags: Vec::new(),
    });
    encode_tile(&[&layer])
}

pub(crate) fn two_named_layers_tile(swapped: bool) -> Vec<u8> {
    let mut roads = LayerBuilder::new("roads");
    let mut line = Vec::new();
    encode_linestring(&mut line, &[(0, 0), (10, 10)]);
    roads.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::LineString,
        geometry: line,
        tags: Vec::new(),
    });
    let mut land = LayerBuilder::new("land");
    let outer = [(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)];
    let mut poly = Vec::new();
    encode_polygon(&mut poly, &[&outer]);
    land.add_feature(Feature {
        id: Some(2),
        geom_type: GeomType::Polygon,
        geometry: poly,
        tags: Vec::new(),
    });
    if swapped {
        encode_tile(&[&land, &roads])
    } else {
        encode_tile(&[&roads, &land])
    }
}

pub(crate) fn duplicate_name_layers_tile(swapped: bool) -> Vec<u8> {
    let mut a = LayerBuilder::new("roads");
    let mut la = Vec::new();
    encode_linestring(&mut la, &[(0, 0), (10, 10)]);
    a.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::LineString,
        geometry: la,
        tags: Vec::new(),
    });
    let mut b = LayerBuilder::new("roads");
    let mut lb = Vec::new();
    encode_linestring(&mut lb, &[(20, 20), (30, 30)]);
    b.add_feature(Feature {
        id: Some(2),
        geom_type: GeomType::LineString,
        geometry: lb,
        tags: Vec::new(),
    });
    if swapped {
        encode_tile(&[&b, &a])
    } else {
        encode_tile(&[&a, &b])
    }
}

pub(crate) fn repeated_feature_tile(paths: &[&[(i32, i32)]]) -> Vec<u8> {
    let mut layer = LayerBuilder::new("roads");
    for path in paths {
        let mut geometry = Vec::new();
        encode_linestring(&mut geometry, path);
        layer.add_feature(Feature {
            id: None,
            geom_type: GeomType::LineString,
            geometry,
            tags: Vec::new(),
        });
    }
    encode_tile(&[&layer])
}

/// Several features sharing one id - the case that forces positional pairing
/// within an id group.
pub(crate) fn duplicate_id_tile(paths: &[&[(i32, i32)]]) -> Vec<u8> {
    let mut layer = LayerBuilder::new("roads");
    let key = layer.intern_key("class");
    let value = layer.intern_value(Value::String("service".to_string()));
    for path in paths {
        let mut geometry = Vec::new();
        encode_linestring(&mut geometry, path);
        layer.add_feature(Feature {
            id: Some(99),
            geom_type: GeomType::LineString,
            geometry,
            tags: vec![(key, value)],
        });
    }
    encode_tile(&[&layer])
}

/// An `ocean` polygon with no id - the layer whose synthetic ids the diff
/// deliberately ignores, so it must match geometrically.
pub(crate) fn anonymous_ocean_tile(rings: &[&[(i32, i32)]]) -> Vec<u8> {
    let mut layer = LayerBuilder::new(mvt::OCEAN_LAYER);
    let mut geometry = Vec::new();
    encode_polygon(&mut geometry, rings);
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::Polygon,
        geometry,
        tags: Vec::new(),
    });
    encode_tile(&[&layer])
}

// --- hand-built layers (shapes a production encoder never emits) ---

pub(crate) fn empty_layer_tile(name: &str, extent: u64) -> Vec<u8> {
    let mut layer = Vec::new();
    encode_bytes_field_always(&mut layer, mvt::layer::NAME, name.as_bytes());
    encode_varint_field_always(&mut layer, mvt::layer::EXTENT, extent);
    encode_varint_field_always(
        &mut layer,
        mvt::layer::VERSION,
        u64::from(mvt::FIXTURE_LAYER_VERSION),
    );
    let mut tile = Vec::new();
    encode_bytes_field_always(&mut tile, mvt::TILE_LAYER, &layer);
    tile
}

pub(crate) fn versioned_layer_tile(version: Option<u64>) -> Vec<u8> {
    let mut layer = Vec::new();
    encode_bytes_field_always(&mut layer, mvt::layer::NAME, b"roads");
    encode_varint_field_always(&mut layer, mvt::layer::EXTENT, u64::from(mvt::TILE_EXTENT));
    if let Some(v) = version {
        encode_varint_field_always(&mut layer, mvt::layer::VERSION, v);
    }
    let mut tile = Vec::new();
    encode_bytes_field_always(&mut tile, mvt::TILE_LAYER, &layer);
    tile
}

pub(crate) fn split_geometry_tile(chunks: &[&[u32]]) -> Vec<u8> {
    let mut feature = Vec::new();
    encode_varint_field_always(&mut feature, mvt::feature::ID, 1);
    encode_varint_field_always(
        &mut feature,
        mvt::feature::GEOMETRY_TYPE,
        u64::from(mvt::GEOMETRY_LINESTRING),
    );
    for chunk in chunks {
        let mut g = Vec::new();
        for &v in *chunk {
            encode_varint(&mut g, u64::from(v));
        }
        encode_bytes_field_always(&mut feature, mvt::feature::GEOMETRY, &g);
    }
    let mut layer = Vec::new();
    encode_bytes_field_always(&mut layer, mvt::layer::NAME, b"roads");
    encode_varint_field_always(&mut layer, mvt::layer::EXTENT, u64::from(mvt::TILE_EXTENT));
    encode_bytes_field_always(&mut layer, mvt::layer::FEATURE, &feature);
    let mut tile = Vec::new();
    encode_bytes_field_always(&mut tile, mvt::TILE_LAYER, &layer);
    tile
}

pub(crate) fn split_tags_tile(chunks: &[&[u32]]) -> Vec<u8> {
    let mut feature = Vec::new();
    encode_varint_field_always(
        &mut feature,
        mvt::feature::GEOMETRY_TYPE,
        u64::from(mvt::GEOMETRY_POINT),
    );
    let mut g = Vec::new();
    for v in [command(mvt::COMMAND_MOVE_TO, 1), zigzag(0), zigzag(0)] {
        encode_varint(&mut g, u64::from(v));
    }
    encode_bytes_field_always(&mut feature, mvt::feature::GEOMETRY, &g);
    for chunk in chunks {
        let mut t = Vec::new();
        for &v in *chunk {
            encode_varint(&mut t, u64::from(v));
        }
        encode_bytes_field_always(&mut feature, mvt::feature::TAGS, &t);
    }
    let mut layer = Vec::new();
    encode_bytes_field_always(&mut layer, mvt::layer::NAME, b"roads");
    encode_varint_field_always(&mut layer, mvt::layer::EXTENT, u64::from(mvt::TILE_EXTENT));
    encode_bytes_field_always(&mut layer, mvt::layer::FEATURE, &feature);
    encode_bytes_field_always(&mut layer, mvt::layer::KEY, b"a");
    encode_bytes_field_always(&mut layer, mvt::layer::KEY, b"b");
    let mut v0 = Vec::new();
    encode_bytes_field_always(&mut v0, mvt::value::STRING, b"x");
    encode_bytes_field_always(&mut layer, mvt::layer::VALUE, &v0);
    let mut v1 = Vec::new();
    encode_bytes_field_always(&mut v1, mvt::value::STRING, b"y");
    encode_bytes_field_always(&mut layer, mvt::layer::VALUE, &v1);
    let mut tile = Vec::new();
    encode_bytes_field_always(&mut tile, mvt::TILE_LAYER, &layer);
    tile
}

// ---------------------------------------------------------------------------
// PMTiles archive fixtures
// ---------------------------------------------------------------------------

/// A per-test scratch directory, removed on drop. The shared allocator enforces
/// unique names across the corpus fixtures and clears stale paths before use.
pub(crate) struct TestDir {
    pub(crate) path: PathBuf,
}

impl TestDir {
    pub(crate) fn new(name: &str) -> Self {
        let path = crate::support::test_scratch::scratch("elivagar-corpus-fixture", name);
        Self { path }
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.path));
    }
}

pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).expect("write gzip");
    encoder.finish().expect("finish gzip")
}

/// Write a PMTiles archive holding the given `(z, x, y, mvt)` tiles.
///
/// Tiles are sorted into Hilbert tile-id order first because the writer requires
/// ascending ids, and identical adjacent payloads are what let it emit a
/// run-length entry - which is exactly the shape the engine's blob-pair grouping
/// is built around.
pub(crate) fn write_archive(path: &Path, mut tiles: Vec<(u8, u32, u32, Vec<u8>)>) {
    tiles.sort_by_key(|(z, x, y, _)| xy_to_tile_id(*z, *x, *y));
    let config = PmtilesConfig::new(
        0,
        2,
        LonLatBbox {
            west: 0.0,
            south: 0.0,
            east: 1.0,
            north: 1.0,
        },
        MapCenter {
            lon: 0.5,
            lat: 0.5,
            zoom: 1,
        },
    )
    .expect("valid fixture geography");
    let mut writer = PmtilesWriter::new(config);
    for (z, x, y, tile) in tiles {
        writer
            .add_tile(z, x, y, &gzip(&tile))
            .expect("add tile to archive");
    }
    writer.write_to(path).expect("write archive");
}
