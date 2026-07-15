use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::Compression;
use flate2::write::GzEncoder;
use protohoggr::{encode_bytes_field_always, encode_varint_field_always};

use super::*;
use crate::mvt::{Feature, GeomType, LayerBuilder, Value, encode_linestring, encode_polygon};
use crate::pmtiles_reader::PmtilesReader;
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

#[derive(Clone, Copy)]
struct ResidualMatchPoint(usize);

fn crossing_residual_cost(ci: usize, bi: usize) -> i32 {
    match (ci, bi) {
        (0, 0) => 1,
        (0, 1) | (1, 0) => 2,
        (1, 1) => 100,
        (ci, bi) if ci == bi => 0,
        _ => 1_000,
    }
}

fn crossing_residual_pairs() -> Vec<(usize, usize)> {
    let current: Vec<_> = (0..9).map(ResidualMatchPoint).collect();
    let baseline: Vec<_> = (0..9).map(ResidualMatchPoint).collect();
    let mut cur_used = vec![false; current.len()];
    let mut bl_used = vec![false; baseline.len()];
    remaining_pairs(
        &current,
        &baseline,
        &mut cur_used,
        &mut bl_used,
        |_| (),
        |_, _| 0,
        |left, right| {
            u64::try_from(crossing_residual_cost(left.0, right.0)).expect("non-negative proxy cost")
        },
        |left, right| crossing_residual_cost(left.0, right.0),
    )
}

#[test]
fn residual_matcher_uses_minimum_cost_assignment_over_greedy_crossing() {
    let pairs = crossing_residual_pairs();
    assert_eq!(
        pairs,
        vec![
            (0, 1),
            (1, 0),
            (2, 2),
            (3, 3),
            (4, 4),
            (5, 5),
            (6, 6),
            (7, 7),
            (8, 8)
        ]
    );
    let total: i32 = pairs
        .iter()
        .map(|&(ci, bi)| crossing_residual_cost(ci, bi))
        .sum();
    assert_eq!(total, 4);
}

#[test]
fn residual_matcher_is_deterministic() {
    let expected = crossing_residual_pairs();
    for _ in 0..16 {
        assert_eq!(crossing_residual_pairs(), expected);
    }
}

// Exhaustive min-cost max-cardinality reference: try every assignment.
fn brute_force_best(costs: &[Vec<Option<i32>>]) -> (usize, i64) {
    fn recurse(
        costs: &[Vec<Option<i32>>],
        ci: usize,
        used: &mut [bool],
        matched: usize,
        cost: i64,
        best: &mut (usize, i64),
    ) {
        if ci == costs.len() {
            if matched > best.0 || (matched == best.0 && cost < best.1) {
                *best = (matched, cost);
            }
            return;
        }
        recurse(costs, ci + 1, used, matched, cost, best);
        for (bi, slot) in costs[ci].iter().enumerate() {
            if let Some(edge) = slot
                && !used[bi]
            {
                used[bi] = true;
                recurse(
                    costs,
                    ci + 1,
                    used,
                    matched + 1,
                    cost + i64::from(*edge),
                    best,
                );
                used[bi] = false;
            }
        }
    }
    let width = costs.first().map_or(0, Vec::len);
    let mut best = (0, i64::MAX);
    recurse(costs, 0, &mut vec![false; width], 0, 0, &mut best);
    if best.0 == 0 {
        best.1 = 0;
    }
    best
}

fn sparse_pairs_for(costs: &[Vec<Option<i32>>]) -> Vec<(usize, usize)> {
    let width = costs.first().map_or(0, Vec::len);
    let current: Vec<_> = (0..costs.len()).map(ResidualMatchPoint).collect();
    let baseline: Vec<_> = (0..width).map(ResidualMatchPoint).collect();
    let mut cur_used = vec![false; current.len()];
    let mut bl_used = vec![false; baseline.len()];
    let mut candidates = Vec::new();
    for (ci, row) in costs.iter().enumerate() {
        for (bi, slot) in row.iter().enumerate() {
            if slot.is_some() {
                candidates.push((ci, bi));
            }
        }
    }
    sparse_min_cost_pairs(
        &current,
        &baseline,
        &mut cur_used,
        &mut bl_used,
        &candidates,
        &|l, r| costs[l.0][r.0].expect("distance is only asked for candidate edges"),
    )
}

#[test]
fn sparse_matcher_matches_brute_force_oracle() {
    fn dense(rows: &[&[i32]]) -> Vec<Vec<Option<i32>>> {
        rows.iter()
            .map(|row| row.iter().map(|&cost| Some(cost)).collect())
            .collect()
    }
    let cases: Vec<Vec<Vec<Option<i32>>>> = vec![
        // Review counterexample: equal-cost alternating structure that broke
        // the tie-relaxing Bellman-Ford (predecessor cycle, endless augment).
        dense(&[&[1, 1, 3, 1], &[0, 0, 2, 3], &[2, 2, 4, 3], &[2, 4, 4, 2]]),
        // All-zero ties: any perfect matching, but it must terminate and
        // stay maximum-cardinality.
        dense(&[&[0, 0, 0], &[0, 0, 0], &[0, 0, 0]]),
        // Crossing: greedy takes 1 then 100; optimum is 2 + 2.
        dense(&[&[1, 2], &[2, 100]]),
        // Rectangular with ties on every row.
        dense(&[&[0, 0, 1], &[0, 1, 0]]),
        // Sparse edges force cardinality-first choices.
        vec![
            vec![Some(5), None, None],
            vec![Some(1), Some(1), None],
            vec![None, Some(0), Some(9)],
        ],
        // Zero-cost alternatives: two ways around at equal cost.
        dense(&[&[0, 1, 0], &[1, 0, 0], &[0, 0, 1]]),
    ];
    for costs in cases {
        let pairs = sparse_pairs_for(&costs);
        let (cardinality, best_cost) = brute_force_best(&costs);
        assert_eq!(pairs.len(), cardinality, "cardinality for {costs:?}");
        let total: i64 = pairs
            .iter()
            .map(|&(ci, bi)| i64::from(costs[ci][bi].expect("paired edge exists")))
            .sum();
        assert_eq!(total, best_cost, "cost for {costs:?}");
    }
}

#[derive(Clone, Copy)]
struct StarvedPoint {
    key: u8,
    cluster: u8,
}

#[test]
fn residual_matcher_exhausts_same_key_pairs_before_force_zip() {
    fn push(list: &mut Vec<StarvedPoint>, key: u8, cluster: u8, n: usize) {
        for _ in 0..n {
            list.push(StarvedPoint { key, cluster });
        }
    }
    // Each key holds a 9-current/8-baseline cluster and an 8-current/9-baseline
    // cluster: the K=8 candidate graph cannot bridge the clusters, so
    // min-cost matching strands one current and one baseline per key. Baseline
    // key order is reversed so a key-blind force-zip would pair the
    // leftovers across keys; the same-key completion sweep must not.
    let mut current = Vec::new();
    let mut baseline = Vec::new();
    push(&mut current, 0, 1, 9);
    push(&mut current, 0, 2, 8);
    push(&mut current, 1, 1, 9);
    push(&mut current, 1, 2, 8);
    push(&mut baseline, 1, 1, 8);
    push(&mut baseline, 1, 2, 9);
    push(&mut baseline, 0, 1, 8);
    push(&mut baseline, 0, 2, 9);
    let mut cur_used = vec![false; current.len()];
    let mut bl_used = vec![false; baseline.len()];
    let cluster_cost = |l: &StarvedPoint, r: &StarvedPoint| -> u16 {
        if l.cluster == r.cluster { 1 } else { 1000 }
    };
    let paired = remaining_pairs(
        &current,
        &baseline,
        &mut cur_used,
        &mut bl_used,
        |point| point.key,
        |_, _| 0,
        |l, r| u64::from(cluster_cost(l, r)),
        |l, r| i32::from(cluster_cost(l, r)),
    );
    assert_eq!(paired.len(), 34);
    for (ci, bi) in paired {
        assert_eq!(
            current[ci].key, baseline[bi].key,
            "pair {ci} {bi} crosses keys"
        );
    }
}

// Both hashes must agree AND deliver the expected verdict: asserting
// agreement alone would let correlated mistakes pass.
fn assert_fingerprint_verdict(left: &[u8], right: &[u8], expect_equal: bool) {
    let detail_equal = detail_tile_hash(&decode_detail_tile(left).expect("decode left"))
        == detail_tile_hash(&decode_detail_tile(right).expect("decode right"));
    let streaming_equal = streaming_tile_hash(left).expect("hash left")
        == streaming_tile_hash(right).expect("hash right");
    assert_eq!(detail_equal, expect_equal, "detail verdict");
    assert_eq!(streaming_equal, expect_equal, "streaming verdict");
}

#[test]
fn streaming_fingerprint_matches_detail_hash_on_geometry() {
    let assert_verdict = assert_fingerprint_verdict;
    let first = &[(0, 0), (10, 10)][..];
    let second = &[(20, 20), (30, 30)][..];
    assert_verdict(
        &multiline_tile(&[first, second]),
        &multiline_tile(&[second, first]),
        true,
    );

    let outer = &[(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)][..];
    let rotated = &[(100, 0), (100, 100), (0, 100), (0, 0), (100, 0)][..];
    assert_verdict(&polygon_tile(&[outer]), &polygon_tile(&[rotated]), false);
    assert_verdict(
        &line_tile(Some(1), "a", &[(0, 0), (10, 10)]),
        &line_tile(Some(1), "a", &[(0, 0), (11, 10)]),
        false,
    );

    // Hole order within a component stays semantic in both hashes.
    let hole_a = &[(10, 10), (10, 20), (20, 20), (20, 10), (10, 10)][..];
    let hole_b = &[(30, 30), (30, 40), (40, 40), (40, 30), (30, 30)][..];
    assert_verdict(
        &polygon_tile(&[outer, hole_a, hole_b]),
        &polygon_tile(&[outer, hole_b, hole_a]),
        false,
    );

    // A zero-area ring classifies as a hole in both decoders; adding one
    // changes the geometry. Four points so encode_polygon_ring keeps it
    // (it drops rings with fewer than two LineTo segments).
    let degenerate = &[(50, 50), (60, 50), (70, 50), (50, 50)][..];
    assert_verdict(
        &polygon_tile(&[outer, degenerate]),
        &polygon_tile(&[outer]),
        false,
    );

    // Multipoint point order stays semantic in both hashes.
    assert_verdict(
        &point_tile(&[(0, 0), (10, 10)]),
        &point_tile(&[(10, 10), (0, 0)]),
        false,
    );

    // A repeated-count line MoveTo (invalid but accepted by the detail
    // decoder) splits into a single-point component plus the continuing
    // path, identically in both decoders: MoveTo{a,b} LineTo{c} must equal
    // the conventional encoding of path [a,c] plus lone point [b].
    let mut repeated = Vec::new();
    repeated.push(crate::mvt::command(1, 2));
    repeated.extend([crate::mvt::zigzag(0), crate::mvt::zigzag(0)]);
    repeated.extend([crate::mvt::zigzag(5), crate::mvt::zigzag(5)]);
    repeated.push(crate::mvt::command(2, 1));
    repeated.extend([crate::mvt::zigzag(5), crate::mvt::zigzag(5)]);
    let mut conventional = Vec::new();
    conventional.push(crate::mvt::command(1, 1));
    conventional.extend([crate::mvt::zigzag(0), crate::mvt::zigzag(0)]);
    conventional.push(crate::mvt::command(2, 1));
    conventional.extend([crate::mvt::zigzag(10), crate::mvt::zigzag(10)]);
    conventional.push(crate::mvt::command(1, 1));
    conventional.extend([crate::mvt::zigzag(-5), crate::mvt::zigzag(-5)]);
    assert_verdict(
        &raw_geometry_tile(GeomType::LineString, repeated),
        &raw_geometry_tile(GeomType::LineString, conventional),
        true,
    );
}

#[test]
fn streaming_fingerprint_matches_detail_hash_on_layers_and_features() {
    let assert_verdict = assert_fingerprint_verdict;
    let first = &[(0, 0), (10, 10)][..];
    let second = &[(20, 20), (30, 30)][..];

    // Distinct layer names: encoding order is erased by both hashes.
    assert_verdict(
        &two_named_layers_tile(false),
        &two_named_layers_tile(true),
        true,
    );

    // Duplicate layer names (invalid MVT, accepted by both decoders): the
    // detail hash stably sorts by name so encounter order stays bound, and
    // the streaming hash must agree - swapped content is a difference.
    assert_verdict(
        &duplicate_name_layers_tile(false),
        &duplicate_name_layers_tile(true),
        false,
    );

    // Feature multiplicity: [A, A, B] vs [A, B, B] differ as multisets.
    assert_verdict(
        &repeated_feature_tile(&[first, first, second]),
        &repeated_feature_tile(&[first, second, second]),
        false,
    );

    // Attribute values are bit-exact: float and double wire types differ
    // even for the same numeric value, and signed zeros differ.
    assert_verdict(
        &float_attr_tile(Value::Float(1.0)),
        &float_attr_tile(Value::Double(1.0)),
        false,
    );
    assert_verdict(
        &float_attr_tile(Value::Float(0.0)),
        &float_attr_tile(Value::Float(-0.0)),
        false,
    );

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
        crate::mvt::encode_tile(&[&layer])
    }
    assert_verdict(&attrs_tile(false), &attrs_tile(true), true);

    let mut ordered = LayerBuilder::new("roads");
    let key = ordered.intern_key("class");
    let value = ordered.intern_value(Value::String("path".to_string()));
    for (id, coords) in [(Some(1), first), (Some(2), second)] {
        let mut geometry = Vec::new();
        encode_linestring(&mut geometry, coords);
        ordered.add_feature(Feature {
            id,
            geom_type: GeomType::LineString,
            geometry,
            tags: vec![(key, value)],
        });
    }
    let mut permuted = LayerBuilder::new("roads");
    let key = permuted.intern_key("class");
    let value = permuted.intern_value(Value::String("path".to_string()));
    for (id, coords) in [(Some(2), second), (Some(1), first)] {
        let mut geometry = Vec::new();
        encode_linestring(&mut geometry, coords);
        permuted.add_feature(Feature {
            id,
            geom_type: GeomType::LineString,
            geometry,
            tags: vec![(key, value)],
        });
    }
    assert_verdict(
        &crate::mvt::encode_tile(&[&ordered]),
        &crate::mvt::encode_tile(&[&permuted]),
        true,
    );
}

// Semantic surface v2: the layer version field, strict unknown-field errors
// at every message level, and packed-field concatenation. Each MUST case is
// asserted on BOTH the streaming hash and the detail decoder (the mirror).
fn versioned_layer_tile(version: Option<u64>) -> Vec<u8> {
    let mut layer = Vec::new();
    encode_bytes_field_always(&mut layer, 1, b"roads");
    encode_varint_field_always(&mut layer, 5, 4096);
    if let Some(v) = version {
        encode_varint_field_always(&mut layer, 15, v);
    }
    let mut tile = Vec::new();
    encode_bytes_field_always(&mut tile, 3, &layer);
    tile
}

#[test]
fn layer_version_is_semantic_with_default_one() {
    // 2 vs 3 differ; an absent version defaults to 1 (equal to explicit 1) but
    // differs from 2 - the exact R2 fix the streaming hash was missing.
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
        // A minimal value message (a string), reused for the Value-level case.
        let mut value = Vec::new();
        encode_bytes_field_always(&mut value, 1, b"x");
        if unknown_at == 3 {
            encode_varint_field_always(&mut value, 9, 1);
        }
        let mut feature = Vec::new();
        encode_varint_field_always(&mut feature, 3, 1);
        if unknown_at == 2 {
            encode_varint_field_always(&mut feature, 9, 1);
        }
        let mut layer = Vec::new();
        encode_bytes_field_always(&mut layer, 1, b"roads");
        encode_varint_field_always(&mut layer, 5, 4096);
        encode_bytes_field_always(&mut layer, 4, &value);
        encode_bytes_field_always(&mut layer, 2, &feature);
        if unknown_at == 1 {
            encode_varint_field_always(&mut layer, 9, 1);
        }
        let mut tile = Vec::new();
        encode_bytes_field_always(&mut tile, 3, &layer);
        if unknown_at == 0 {
            encode_varint_field_always(&mut tile, 9, 1);
        }
        tile
    };
    for level in 0..=3 {
        let tile = level_tile(level);
        assert!(
            streaming_tile_hash(&tile).is_err(),
            "streaming must reject unknown field at level {level}"
        );
        assert!(
            decode_detail_tile(&tile).is_err(),
            "detail decoder must reject unknown field at level {level}"
        );
    }
}

fn split_geometry_tile(chunks: &[&[u32]]) -> Vec<u8> {
    let mut feature = Vec::new();
    encode_varint_field_always(&mut feature, 1, 1);
    encode_varint_field_always(&mut feature, 3, 2); // LineString
    for chunk in chunks {
        let mut g = Vec::new();
        for &v in *chunk {
            protohoggr::encode_varint(&mut g, u64::from(v));
        }
        encode_bytes_field_always(&mut feature, 4, &g);
    }
    let mut layer = Vec::new();
    encode_bytes_field_always(&mut layer, 1, b"roads");
    encode_varint_field_always(&mut layer, 5, 4096);
    encode_bytes_field_always(&mut layer, 2, &feature);
    let mut tile = Vec::new();
    encode_bytes_field_always(&mut tile, 3, &layer);
    tile
}

fn split_tags_tile(chunks: &[&[u32]]) -> Vec<u8> {
    let mut feature = Vec::new();
    encode_varint_field_always(&mut feature, 3, 1); // point
    let mut g = Vec::new();
    for v in [
        crate::mvt::command(1, 1),
        crate::mvt::zigzag(0),
        crate::mvt::zigzag(0),
    ] {
        protohoggr::encode_varint(&mut g, u64::from(v));
    }
    encode_bytes_field_always(&mut feature, 4, &g);
    for chunk in chunks {
        let mut t = Vec::new();
        for &v in *chunk {
            protohoggr::encode_varint(&mut t, u64::from(v));
        }
        encode_bytes_field_always(&mut feature, 2, &t);
    }
    let mut layer = Vec::new();
    encode_bytes_field_always(&mut layer, 1, b"roads");
    encode_varint_field_always(&mut layer, 5, 4096);
    encode_bytes_field_always(&mut layer, 2, &feature);
    encode_bytes_field_always(&mut layer, 3, b"a");
    encode_bytes_field_always(&mut layer, 3, b"b");
    let mut v0 = Vec::new();
    encode_bytes_field_always(&mut v0, 1, b"x");
    encode_bytes_field_always(&mut layer, 4, &v0);
    let mut v1 = Vec::new();
    encode_bytes_field_always(&mut v1, 1, b"y");
    encode_bytes_field_always(&mut layer, 4, &v1);
    let mut tile = Vec::new();
    encode_bytes_field_always(&mut tile, 3, &layer);
    tile
}

#[test]
fn packed_fields_split_across_occurrences_hash_identically() {
    // Geometry (field 4): MoveTo(0,0) then LineTo(10,10), whole vs split.
    let mv = crate::mvt::command(1, 1);
    let lt = crate::mvt::command(2, 1);
    let z0 = crate::mvt::zigzag(0);
    let z10 = crate::mvt::zigzag(10);
    let whole_geom = [mv, z0, z0, lt, z10, z10];
    let head = [mv, z0, z0];
    let tail = [lt, z10, z10];
    assert_fingerprint_verdict(
        &split_geometry_tile(&[&whole_geom]),
        &split_geometry_tile(&[&head, &tail]),
        true,
    );
    // Tags (field 2): key0=val0, key1=val1, whole vs split.
    assert_fingerprint_verdict(
        &split_tags_tile(&[&[0, 0, 1, 1]]),
        &split_tags_tile(&[&[0, 0], &[1, 1]]),
        true,
    );
}

fn point_tile(points: &[(i32, i32)]) -> Vec<u8> {
    let mut geometry = Vec::new();
    geometry.push(crate::mvt::command(
        1,
        u32::try_from(points.len()).expect("point count fits u32"),
    ));
    let (mut cx, mut cy) = (0, 0);
    for &(x, y) in points {
        geometry.push(crate::mvt::zigzag(x - cx));
        geometry.push(crate::mvt::zigzag(y - cy));
        cx = x;
        cy = y;
    }
    raw_geometry_tile(GeomType::Point, geometry)
}

fn raw_geometry_tile(geom_type: GeomType, geometry: Vec<u32>) -> Vec<u8> {
    let mut layer = LayerBuilder::new("raw");
    layer.add_feature(Feature {
        id: Some(1),
        geom_type,
        geometry,
        tags: Vec::new(),
    });
    crate::mvt::encode_tile(&[&layer])
}

fn two_named_layers_tile(swapped: bool) -> Vec<u8> {
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
        crate::mvt::encode_tile(&[&land, &roads])
    } else {
        crate::mvt::encode_tile(&[&roads, &land])
    }
}

fn duplicate_name_layers_tile(swapped: bool) -> Vec<u8> {
    let mut a = LayerBuilder::new("roads");
    let mut line_a = Vec::new();
    encode_linestring(&mut line_a, &[(0, 0), (10, 10)]);
    a.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::LineString,
        geometry: line_a,
        tags: Vec::new(),
    });
    let mut b = LayerBuilder::new("roads");
    let mut line_b = Vec::new();
    encode_linestring(&mut line_b, &[(20, 20), (30, 30)]);
    b.add_feature(Feature {
        id: Some(2),
        geom_type: GeomType::LineString,
        geometry: line_b,
        tags: Vec::new(),
    });
    if swapped {
        crate::mvt::encode_tile(&[&b, &a])
    } else {
        crate::mvt::encode_tile(&[&a, &b])
    }
}

fn repeated_feature_tile(paths: &[&[(i32, i32)]]) -> Vec<u8> {
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
    crate::mvt::encode_tile(&[&layer])
}

#[test]
fn identical_archive_report_passes() {
    let dir = TestDir::new("identical");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let tile = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(&current, vec![(0, 0, 0, tile.clone())]);
    write_archive(&baseline, vec![(0, 0, 0, tile)]);
    let cfg = RegressConfig::default();
    let report = regress(&current, &baseline, &cfg).expect("regress");
    assert!(report.passed(&cfg));
    assert_eq!(report.identical_tiles, 1);
}

#[test]
fn one_tile_removed_reports_only_in_baseline() {
    let dir = TestDir::new("removed");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let tile = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(&current, vec![(1, 0, 0, tile.clone())]);
    write_archive(&baseline, vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile)]);
    let cfg = RegressConfig::default();
    let report = regress(&current, &baseline, &cfg).expect("regress");
    assert_eq!(report.totals.only_in_baseline, 1);
    assert!(!report.passed(&cfg));
}

#[test]
fn moved_vertex_respects_tolerance() {
    let dir = TestDir::new("tolerance");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    write_archive(
        &current,
        vec![(0, 0, 0, line_tile(Some(1), "a", &[(0, 0), (13, 10)]))],
    );
    write_archive(
        &baseline,
        vec![(0, 0, 0, line_tile(Some(1), "a", &[(0, 0), (10, 10)]))],
    );

    let cfg = RegressConfig {
        tol: 4,
        max_moved: 1,
        max_examples: 20,
    };
    let report = regress(&current, &baseline, &cfg).expect("regress tol 4");
    assert_eq!(report.totals.tolerance_moved, 1);
    assert!(report.passed(&cfg));

    let cfg = RegressConfig {
        tol: 2,
        max_moved: 1,
        max_examples: 20,
    };
    let report = regress(&current, &baseline, &cfg).expect("regress tol 2");
    assert_eq!(report.totals.structural_moved, 1);
    assert!(!report.passed(&cfg));
}

#[test]
fn attr_change_reports_attr_changed() {
    let dir = TestDir::new("attr");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    write_archive(
        &current,
        vec![(0, 0, 0, line_tile(Some(1), "b", &[(0, 0), (10, 10)]))],
    );
    write_archive(
        &baseline,
        vec![(0, 0, 0, line_tile(Some(1), "a", &[(0, 0), (10, 10)]))],
    );
    let cfg = RegressConfig::default();
    let report = regress(&current, &baseline, &cfg).expect("regress");
    assert_eq!(report.totals.attr_changed, 1);
}

#[test]
fn layer_present_empty_on_one_side_reports_removed() {
    let dir = TestDir::new("empty-layer");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    write_archive(&current, vec![(0, 0, 0, Vec::new())]);
    write_archive(&baseline, vec![(0, 0, 0, empty_layer_tile("empty", 4096))]);
    let cfg = RegressConfig::default();
    let report = regress(&current, &baseline, &cfg).expect("regress");
    assert_eq!(report.totals.layers_removed, 1);
}

#[test]
fn extent_mismatch_skips_geometry() {
    let dir = TestDir::new("extent");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    write_archive(&current, vec![(0, 0, 0, empty_layer_tile("roads", 8192))]);
    write_archive(&baseline, vec![(0, 0, 0, empty_layer_tile("roads", 4096))]);
    let cfg = RegressConfig::default();
    let report = regress(&current, &baseline, &cfg).expect("regress");
    assert_eq!(report.totals.extent_mismatch, 1);
    assert_eq!(report.totals.structural_moved, 0);
}

#[test]
fn polygon_hole_reassigned_at_zero_distance_is_structural() {
    let dir = TestDir::new("hole");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let outer_a = &[(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)][..];
    let hole_a = &[(20, 20), (20, 40), (40, 40), (40, 20), (20, 20)][..];
    let outer_b = &[(200, 200), (300, 200), (300, 300), (200, 300), (200, 200)][..];
    write_archive(
        &current,
        vec![(0, 0, 0, polygon_tile(&[outer_a, outer_b, hole_a]))],
    );
    write_archive(
        &baseline,
        vec![(0, 0, 0, polygon_tile(&[outer_a, hole_a, outer_b]))],
    );
    let cfg = RegressConfig {
        tol: 10,
        max_moved: 10,
        max_examples: 20,
    };
    let report = regress(&current, &baseline, &cfg).expect("regress");
    assert_eq!(report.totals.structural_moved, 1);
    assert_eq!(report.totals.tolerance_moved, 0);
}

#[test]
fn polygon_hole_escaping_its_outer_is_structural_not_tolerance() {
    // Equal ring counts, equal roles, displacement 2px under tol 3: only the
    // hole-containment predicate can classify this pair as structural. The
    // hole's first vertex crosses the outer boundary (99 -> 101 with the
    // outer ending at x=100), so containment differs while every other
    // structural signal matches; a tolerance verdict would mean the
    // containment branch was skipped.
    let dir = TestDir::new("hole-containment");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let outer = &[(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)][..];
    let hole_inside = &[(99, 50), (89, 50), (89, 60), (99, 60), (99, 50)][..];
    let hole_escaped = &[(101, 50), (91, 50), (91, 60), (101, 60), (101, 50)][..];
    write_archive(
        &current,
        vec![(0, 0, 0, polygon_tile(&[outer, hole_inside]))],
    );
    write_archive(
        &baseline,
        vec![(0, 0, 0, polygon_tile(&[outer, hole_escaped]))],
    );
    let cfg = RegressConfig {
        tol: 3,
        max_moved: 10,
        max_examples: 20,
    };
    let report = regress(&current, &baseline, &cfg).expect("regress");
    assert_eq!(report.totals.structural_moved, 1);
    assert_eq!(report.totals.tolerance_moved, 0);
}

#[test]
fn run_length_directory_preserves_each_addressed_tile() {
    let dir = TestDir::new("run-length");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let tile = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(
        &current,
        vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile.clone())],
    );
    write_archive(&baseline, vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile)]);
    let mut reader = PmtilesReader::open(&current).expect("open current");
    let runs = reader.read_all_runs().expect("read runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_length, 2);

    let cfg = RegressConfig::default();
    let report = regress(&current, &baseline, &cfg).expect("regress");
    assert_eq!(report.identical_tiles, 2);
}

#[test]
fn deduplicated_run_collapses_to_one_raw_pair() {
    let dir = TestDir::new("dedup");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let tile = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(
        &current,
        vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile.clone())],
    );
    write_archive(&baseline, vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile)]);
    let cfg = RegressConfig::default();
    let report = regress(&current, &baseline, &cfg).expect("regress");
    assert_eq!(report.counters.unique_blob_pairs, 1);
    assert_eq!(report.counters.raw_equal_pairs, 1);
    assert_eq!(report.counters.raw_equal_tiles, 2);
    assert_eq!(report.identical_tiles, 2);
}

#[test]
fn detailed_pair_multiplicity_is_reported_per_addressed_tile() {
    let dir = TestDir::new("detailed-pair-multiplicity");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let moved = line_tile(Some(1), "a", &[(0, 0), (13, 10)]);
    let original = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(&current, vec![(1, 0, 0, moved.clone()), (1, 0, 1, moved)]);
    write_archive(
        &baseline,
        vec![(1, 0, 0, original.clone()), (1, 0, 1, original)],
    );
    let cfg = RegressConfig {
        tol: 4,
        max_moved: 2,
        max_examples: 20,
    };
    let report = regress(&current, &baseline, &cfg).expect("regress");
    assert_eq!(report.counters.unique_blob_pairs, 1);
    assert_eq!(report.counters.detailed_pairs, 1);
    assert_eq!(report.counters.detailed_tiles, 2);
    assert_eq!(report.totals.tolerance_moved, 2);
}

#[test]
fn canonical_edge_cases_have_expected_live_engine_outcomes() {
    let dir = TestDir::new("differential-edge-cases");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
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
        &baseline,
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
    let report = regress(&current, &baseline, &cfg).expect("regress");
    // Identical: the permuted multiline and the permuted duplicate-id tile.
    // attr_changed: the bit-distinct NaN floats. tolerance_moved: the ocean
    // polygon shifted 2px under tol 3. structural_moved: moving hole_a after
    // outer_b reattaches it to a different outer (MVT holes bind to the
    // preceding outer ring), so hole containment differs between archives.
    assert_eq!(report.identical_tiles, 2);
    assert_eq!(report.diff_count, 3);
    assert_eq!(
        report.totals,
        DiffTotals {
            attr_changed: 1,
            tolerance_moved: 1,
            structural_moved: 1,
            ..DiffTotals::default()
        }
    );
}
