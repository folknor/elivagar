use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::Compression;
use flate2::write::GzEncoder;
use protohoggr::{encode_bytes_field_always, encode_varint_field_always};

use super::*;
use crate::mvt::{Feature, GeomType, LayerBuilder, Value, encode_linestring, encode_polygon};
use crate::pmtiles_reader::{PmtilesReader, expand_entries};
use crate::pmtiles_writer::{PmtilesConfig, PmtilesWriter, xy_to_tile_id};

static TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(name: &str) -> Self {
        let id = TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join("regress-tests")
            .join(format!("{name}-{id}"));
        fs::create_dir_all(&path).expect("create test dir");
        Self { path }
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.path));
    }
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    std::io::Write::write_all(&mut encoder, data).expect("write gzip");
    encoder.finish().expect("finish gzip")
}

fn config() -> PmtilesConfig {
    PmtilesConfig {
        min_zoom: 0,
        max_zoom: 2,
        bounds: (0.0, 0.0, 1.0, 1.0),
        center: (0.5, 0.5, 1),
    }
}

fn write_archive(path: &Path, mut tiles: Vec<(u8, u32, u32, Vec<u8>)>) {
    tiles.sort_by_key(|(z, x, y, _)| xy_to_tile_id(*z, *x, *y));
    let mut writer = PmtilesWriter::new(config());
    for (z, x, y, tile) in tiles {
        writer
            .add_tile(z, x, y, &gzip(&tile))
            .expect("add tile to archive");
    }
    writer.write_to(path).expect("write archive");
}

fn line_tile(id: Option<u64>, attr: &str, coords: &[(i32, i32)]) -> Vec<u8> {
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
    crate::mvt::encode_tile(&[&layer])
}

fn two_layer_tile() -> (Vec<u8>, Vec<Vec<(i32, i32)>>) {
    let mut roads = LayerBuilder::new("roads");
    let key = roads.intern_key("class");
    let val = roads.intern_value(Value::String("primary".to_string()));
    let mut line = Vec::new();
    encode_linestring(&mut line, &[(10, 10), (20, 20)]);
    roads.add_feature(Feature {
        id: Some(9),
        geom_type: GeomType::LineString,
        geometry: line,
        tags: vec![(key, val)],
    });

    let outer = [(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)];
    let hole = [(20, 20), (20, 40), (40, 40), (40, 20), (20, 20)];
    let mut land = LayerBuilder::new("land");
    let lk = land.intern_key("kind");
    let lv = land.intern_value(Value::String("park".to_string()));
    let mut poly = Vec::new();
    encode_polygon(&mut poly, &[&outer, &hole]);
    let decoded_poly = crate::geometry::decode_mvt_polygon(&poly);
    land.add_feature(Feature {
        id: Some(7),
        geom_type: GeomType::Polygon,
        geometry: poly,
        tags: vec![(lk, lv)],
    });

    (crate::mvt::encode_tile(&[&roads, &land]), decoded_poly)
}

fn empty_layer_tile(name: &str, extent: u64) -> Vec<u8> {
    let mut layer = Vec::new();
    encode_bytes_field_always(&mut layer, 1, name.as_bytes());
    encode_varint_field_always(&mut layer, 5, extent);
    encode_varint_field_always(&mut layer, 15, 2);
    let mut tile = Vec::new();
    encode_bytes_field_always(&mut tile, 3, &layer);
    tile
}

fn float_attr_tile(value: Value) -> Vec<u8> {
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
    crate::mvt::encode_tile(&[&layer])
}

fn multiline_tile(paths: &[&[(i32, i32)]]) -> Vec<u8> {
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
    crate::mvt::encode_tile(&[&layer])
}

fn encode_multiline(buf: &mut Vec<u32>, paths: &[&[(i32, i32)]]) {
    buf.clear();
    let mut cx = 0i32;
    let mut cy = 0i32;
    for path in paths {
        if path.len() < 2 {
            continue;
        }
        buf.push(crate::mvt::command(1, 1));
        buf.push(crate::mvt::zigzag(path[0].0 - cx));
        buf.push(crate::mvt::zigzag(path[0].1 - cy));
        cx = path[0].0;
        cy = path[0].1;
        let line_count = u32::try_from(path.len() - 1).expect("path length fits u32");
        buf.push(crate::mvt::command(2, line_count));
        for &(x, y) in &path[1..] {
            buf.push(crate::mvt::zigzag(x - cx));
            buf.push(crate::mvt::zigzag(y - cy));
            cx = x;
            cy = y;
        }
    }
}

fn polygon_tile(rings: &[&[(i32, i32)]]) -> Vec<u8> {
    let mut layer = LayerBuilder::new("land");
    let mut geom = Vec::new();
    encode_polygon(&mut geom, rings);
    layer.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::Polygon,
        geometry: geom,
        tags: Vec::new(),
    });
    crate::mvt::encode_tile(&[&layer])
}

fn duplicate_id_tile(paths: &[&[(i32, i32)]]) -> Vec<u8> {
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
    crate::mvt::encode_tile(&[&layer])
}

fn anonymous_ocean_tile(rings: &[&[(i32, i32)]]) -> Vec<u8> {
    let mut layer = LayerBuilder::new("ocean");
    let mut geometry = Vec::new();
    encode_polygon(&mut geometry, rings);
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::Polygon,
        geometry,
        tags: Vec::new(),
    });
    crate::mvt::encode_tile(&[&layer])
}

#[test]
fn canonical_decoder_round_trips_two_layer_tile() {
    let (tile, decoded_poly) = two_layer_tile();
    let canon = decode_canonical(&tile).expect("decode canonical tile");
    assert_eq!(canon.layers.len(), 2);
    let land = canon
        .layers
        .iter()
        .find(|layer| layer.name == "land")
        .expect("land layer");
    assert_eq!(land.extent, 4096);
    assert_eq!(land.features[0].id, Some(7));
    assert_eq!(
        land.features[0].attrs,
        vec![("kind".to_string(), AttrVal::String("park".to_string()))]
    );
    let rings: Vec<_> = land.features[0].components[0]
        .rings
        .iter()
        .map(|ring| ring.points.clone())
        .collect();
    assert_eq!(rings, decoded_poly);
}

#[test]
fn canonicalization_erases_feature_order() {
    let mut layer_a = LayerBuilder::new("roads");
    let key_a = layer_a.intern_key("class");
    let val_a = layer_a.intern_value(Value::String("path".to_string()));
    for (id, coords) in [
        (Some(2), &[(20, 20), (30, 30)][..]),
        (Some(1), &[(0, 0), (10, 10)][..]),
    ] {
        let mut geom = Vec::new();
        encode_linestring(&mut geom, coords);
        layer_a.add_feature(Feature {
            id,
            geom_type: GeomType::LineString,
            geometry: geom,
            tags: vec![(key_a, val_a)],
        });
    }

    let mut layer_b = LayerBuilder::new("roads");
    let key_b = layer_b.intern_key("class");
    let val_b = layer_b.intern_value(Value::String("path".to_string()));
    for (id, coords) in [
        (Some(1), &[(0, 0), (10, 10)][..]),
        (Some(2), &[(20, 20), (30, 30)][..]),
    ] {
        let mut geom = Vec::new();
        encode_linestring(&mut geom, coords);
        layer_b.add_feature(Feature {
            id,
            geom_type: GeomType::LineString,
            geometry: geom,
            tags: vec![(key_b, val_b)],
        });
    }

    let a = decode_canonical(&crate::mvt::encode_tile(&[&layer_a])).expect("decode a");
    let b = decode_canonical(&crate::mvt::encode_tile(&[&layer_b])).expect("decode b");
    assert_eq!(a, b);
}

#[test]
fn canon_hash_catches_geometry_attrs_features_and_attr_bits() {
    let base = decode_canonical(&line_tile(Some(1), "a", &[(0, 0), (10, 10)])).expect("base");
    let moved = decode_canonical(&line_tile(Some(1), "a", &[(0, 0), (11, 10)])).expect("moved");
    let attr = decode_canonical(&line_tile(Some(1), "b", &[(0, 0), (10, 10)])).expect("attr");
    let dropped = decode_canonical(&empty_layer_tile("roads", 4096)).expect("dropped");
    assert_ne!(canon_hash(&base), canon_hash(&moved));
    assert_ne!(canon_hash(&base), canon_hash(&attr));
    assert_ne!(canon_hash(&base), canon_hash(&dropped));

    let float = decode_canonical(&float_attr_tile(Value::Float(1.0))).expect("float");
    let double = decode_canonical(&float_attr_tile(Value::Double(1.0))).expect("double");
    assert_ne!(canon_hash(&float), canon_hash(&double));

    let nan_a = decode_canonical(&float_attr_tile(Value::Float(f32::from_bits(0x7fc0_0001))))
        .expect("nan a");
    let nan_b = decode_canonical(&float_attr_tile(Value::Float(f32::from_bits(0x7fc0_0002))))
        .expect("nan b");
    assert_ne!(canon_hash(&nan_a), canon_hash(&nan_b));

    let pos_zero = decode_canonical(&float_attr_tile(Value::Float(0.0))).expect("pos zero");
    let neg_zero = decode_canonical(&float_attr_tile(Value::Float(-0.0))).expect("neg zero");
    assert_ne!(canon_hash(&pos_zero), canon_hash(&neg_zero));
}

#[test]
fn canonicalization_erases_merged_component_order_but_not_polygon_ring_order() {
    let p1 = &[(0, 0), (10, 10)][..];
    let p2 = &[(20, 20), (30, 30)][..];
    let a = decode_canonical(&multiline_tile(&[p1, p2])).expect("decode a");
    let b = decode_canonical(&multiline_tile(&[p2, p1])).expect("decode b");
    assert_eq!(a, b);

    let outer = &[(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)][..];
    let hole_a = &[(10, 10), (10, 20), (20, 20), (20, 10), (10, 10)][..];
    let hole_b = &[(30, 30), (30, 40), (40, 40), (40, 30), (30, 30)][..];
    let poly_a = decode_canonical(&polygon_tile(&[outer, hole_a, hole_b])).expect("poly a");
    let poly_b = decode_canonical(&polygon_tile(&[outer, hole_b, hole_a])).expect("poly b");
    assert_ne!(poly_a, poly_b);
}

#[test]
fn identical_archive_report_passes() {
    let dir = TestDir::new("identical");
    let current = dir.path.join("current.pmtiles");
    let blessed = dir.path.join("blessed.pmtiles");
    let tile = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(&current, vec![(0, 0, 0, tile.clone())]);
    write_archive(&blessed, vec![(0, 0, 0, tile)]);
    let cfg = RegressConfig::default();
    let report = regress(&current, &blessed, &cfg).expect("regress");
    assert!(report.passed(&cfg));
    assert_eq!(report.identical_tiles, 1);
}

#[test]
fn one_tile_removed_reports_only_in_blessed() {
    let dir = TestDir::new("removed");
    let current = dir.path.join("current.pmtiles");
    let blessed = dir.path.join("blessed.pmtiles");
    let tile = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(&current, vec![(1, 0, 0, tile.clone())]);
    write_archive(&blessed, vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile)]);
    let cfg = RegressConfig::default();
    let report = regress(&current, &blessed, &cfg).expect("regress");
    assert_eq!(report.totals.only_in_blessed, 1);
    assert!(!report.passed(&cfg));
}

#[test]
fn moved_vertex_respects_tolerance() {
    let dir = TestDir::new("tolerance");
    let current = dir.path.join("current.pmtiles");
    let blessed = dir.path.join("blessed.pmtiles");
    write_archive(
        &current,
        vec![(0, 0, 0, line_tile(Some(1), "a", &[(0, 0), (13, 10)]))],
    );
    write_archive(
        &blessed,
        vec![(0, 0, 0, line_tile(Some(1), "a", &[(0, 0), (10, 10)]))],
    );

    let cfg = RegressConfig {
        tol: 4,
        max_moved: 1,
        max_examples: 20,
    };
    let report = regress(&current, &blessed, &cfg).expect("regress tol 4");
    assert_eq!(report.totals.tolerance_moved, 1);
    assert!(report.passed(&cfg));

    let cfg = RegressConfig {
        tol: 2,
        max_moved: 1,
        max_examples: 20,
    };
    let report = regress(&current, &blessed, &cfg).expect("regress tol 2");
    assert_eq!(report.totals.structural_moved, 1);
    assert!(!report.passed(&cfg));
}

#[test]
fn attr_change_reports_attr_changed() {
    let dir = TestDir::new("attr");
    let current = dir.path.join("current.pmtiles");
    let blessed = dir.path.join("blessed.pmtiles");
    write_archive(
        &current,
        vec![(0, 0, 0, line_tile(Some(1), "b", &[(0, 0), (10, 10)]))],
    );
    write_archive(
        &blessed,
        vec![(0, 0, 0, line_tile(Some(1), "a", &[(0, 0), (10, 10)]))],
    );
    let cfg = RegressConfig::default();
    let report = regress(&current, &blessed, &cfg).expect("regress");
    assert_eq!(report.totals.attr_changed, 1);
}

#[test]
fn layer_present_empty_on_one_side_reports_removed() {
    let dir = TestDir::new("empty-layer");
    let current = dir.path.join("current.pmtiles");
    let blessed = dir.path.join("blessed.pmtiles");
    write_archive(&current, vec![(0, 0, 0, Vec::new())]);
    write_archive(&blessed, vec![(0, 0, 0, empty_layer_tile("empty", 4096))]);
    let cfg = RegressConfig::default();
    let report = regress(&current, &blessed, &cfg).expect("regress");
    assert_eq!(report.totals.layers_removed, 1);
}

#[test]
fn extent_mismatch_skips_geometry() {
    let dir = TestDir::new("extent");
    let current = dir.path.join("current.pmtiles");
    let blessed = dir.path.join("blessed.pmtiles");
    write_archive(&current, vec![(0, 0, 0, empty_layer_tile("roads", 8192))]);
    write_archive(&blessed, vec![(0, 0, 0, empty_layer_tile("roads", 4096))]);
    let cfg = RegressConfig::default();
    let report = regress(&current, &blessed, &cfg).expect("regress");
    assert_eq!(report.totals.extent_mismatch, 1);
    assert_eq!(report.totals.structural_moved, 0);
}

#[test]
fn polygon_hole_reassigned_at_zero_distance_is_structural() {
    let dir = TestDir::new("hole");
    let current = dir.path.join("current.pmtiles");
    let blessed = dir.path.join("blessed.pmtiles");
    let outer_a = &[(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)][..];
    let hole_a = &[(20, 20), (20, 40), (40, 40), (40, 20), (20, 20)][..];
    let outer_b = &[(200, 200), (300, 200), (300, 300), (200, 300), (200, 200)][..];
    write_archive(
        &current,
        vec![(0, 0, 0, polygon_tile(&[outer_a, outer_b, hole_a]))],
    );
    write_archive(
        &blessed,
        vec![(0, 0, 0, polygon_tile(&[outer_a, hole_a, outer_b]))],
    );
    let cfg = RegressConfig {
        tol: 10,
        max_moved: 10,
        max_examples: 20,
    };
    let report = regress(&current, &blessed, &cfg).expect("regress");
    assert_eq!(report.totals.structural_moved, 1);
    assert_eq!(report.totals.tolerance_moved, 0);
}

#[test]
fn run_length_directory_expansion_compares_each_addressed_tile() {
    let dir = TestDir::new("run-length");
    let current = dir.path.join("current.pmtiles");
    let blessed = dir.path.join("blessed.pmtiles");
    let tile = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(
        &current,
        vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile.clone())],
    );
    write_archive(&blessed, vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile)]);
    let mut reader = PmtilesReader::open(&current).expect("open current");
    let root = reader
        .read_directory(reader.root_dir_offset(), reader.root_dir_length())
        .expect("read root dir");
    let mut expanded = Vec::new();
    expand_entries(&root, &mut expanded);
    assert_eq!(expanded.len(), 2);
    assert_eq!(expanded[0].offset, expanded[1].offset);

    let cfg = RegressConfig::default();
    let report = regress(&current, &blessed, &cfg).expect("regress");
    assert_eq!(report.identical_tiles, 2);
}

#[test]
fn deduplicated_run_collapses_to_one_raw_pair() {
    let dir = TestDir::new("dedup");
    let current = dir.path.join("current.pmtiles");
    let blessed = dir.path.join("blessed.pmtiles");
    let tile = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(
        &current,
        vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile.clone())],
    );
    write_archive(&blessed, vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile)]);
    let cfg = RegressConfig::default();
    let report = regress(&current, &blessed, &cfg).expect("regress");
    assert_eq!(report.counters.unique_blob_pairs, 1);
    assert_eq!(report.counters.raw_equal_pairs, 1);
    assert_eq!(report.counters.raw_equal_tiles, 2);
    assert_eq!(report.identical_tiles, 2);
}

#[test]
fn detailed_pair_multiplicity_and_legacy_oracle_match() {
    let dir = TestDir::new("detailed-pair-multiplicity");
    let current = dir.path.join("current.pmtiles");
    let blessed = dir.path.join("blessed.pmtiles");
    let moved = line_tile(Some(1), "a", &[(0, 0), (13, 10)]);
    let original = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(&current, vec![(1, 0, 0, moved.clone()), (1, 0, 1, moved)]);
    write_archive(
        &blessed,
        vec![(1, 0, 0, original.clone()), (1, 0, 1, original)],
    );
    let cfg = RegressConfig {
        tol: 4,
        max_moved: 2,
        max_examples: 20,
    };
    let report = regress(&current, &blessed, &cfg).expect("regress");
    assert_eq!(report.counters.unique_blob_pairs, 1);
    assert_eq!(report.counters.detailed_pairs, 1);
    assert_eq!(report.counters.detailed_tiles, 2);
    assert_eq!(report.totals.tolerance_moved, 2);
    assert_regress_differential_oracle(&current, &blessed, &cfg).expect("differential oracle");
}

#[test]
fn differential_oracle_covers_canonical_edge_cases() {
    let dir = TestDir::new("differential-edge-cases");
    let current = dir.path.join("current.pmtiles");
    let blessed = dir.path.join("blessed.pmtiles");
    let p1 = &[(0, 0), (10, 10)][..];
    let p2 = &[(20, 20), (30, 30)][..];
    let outer_a = &[(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)][..];
    let hole_a = &[(20, 20), (20, 40), (40, 40), (40, 20), (20, 20)][..];
    let outer_b = &[(200, 200), (300, 200), (300, 300), (200, 300), (200, 200)][..];
    let ocean_a = &[(0, 0), (80, 0), (80, 80), (0, 80), (0, 0)][..];
    let ocean_b = &[(2, 0), (82, 0), (82, 80), (2, 80), (2, 0)][..];
    write_archive(
        &current,
        vec![
            (2, 0, 0, multiline_tile(&[p1, p2])),
            (
                2,
                0,
                1,
                float_attr_tile(Value::Float(f32::from_bits(0x7fc0_0001))),
            ),
            (2, 1, 0, duplicate_id_tile(&[p1, p2])),
            (2, 1, 1, anonymous_ocean_tile(&[ocean_a])),
            (2, 2, 0, polygon_tile(&[outer_a, outer_b, hole_a])),
        ],
    );
    write_archive(
        &blessed,
        vec![
            (2, 0, 0, multiline_tile(&[p2, p1])),
            (
                2,
                0,
                1,
                float_attr_tile(Value::Float(f32::from_bits(0x7fc0_0002))),
            ),
            (2, 1, 0, duplicate_id_tile(&[p2, p1])),
            (2, 1, 1, anonymous_ocean_tile(&[ocean_b])),
            (2, 2, 0, polygon_tile(&[outer_a, hole_a, outer_b])),
        ],
    );
    let cfg = RegressConfig {
        tol: 3,
        max_moved: 10,
        max_examples: 20,
    };
    assert_regress_differential_oracle(&current, &blessed, &cfg).expect("differential oracle");
}
