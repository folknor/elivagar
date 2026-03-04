use super::*;
use crate::shortbread::{AttrValue, GeomExpect, Layer, LayerMatch};
use smallvec::smallvec;
use std::borrow::Cow;

#[test]
fn flat_index_guard_sorted_large_allowed() {
    let mode = select_node_store_mode(
        false,
        true,
        false,
        50 * 1024 * 1024 * 1024,
        false,
    ).expect("sorted input should be allowed");
    assert_eq!(mode, NodeStoreMode::Sorted);
}

#[test]
fn flat_index_guard_unsorted_small_allowed() {
    let mode = select_node_store_mode(
        false,
        false,
        false,
        512 * 1024 * 1024,
        false,
    ).expect("small unsorted input should be allowed");
    assert_eq!(mode, NodeStoreMode::Flat { unsafe_override: false });
}

#[test]
fn flat_index_guard_unsorted_large_rejected_with_stable_error() {
    let err = select_node_store_mode(
        false,
        false,
        false,
        2 * 1024 * 1024 * 1024,
        false,
    ).expect_err("large unsorted input should be rejected");
    let msg = err.to_string();
    assert!(msg.contains("does not declare Sort.Type_then_ID"));
    assert!(msg.contains("pbfhogg sort input.pbf -o sorted.pbf"));
    assert!(msg.contains("--force-sorted"));
}

#[test]
fn flat_index_guard_override_path_works() {
    let mode = select_node_store_mode(
        false,
        false,
        false,
        2 * 1024 * 1024 * 1024,
        true,
    ).expect("override should allow large unsorted input");
    assert_eq!(mode, NodeStoreMode::Flat { unsafe_override: true });
}

#[test]
fn missing_ref_stats_accumulates_and_snapshots() {
    let stats = MissingRefStatsAtomic::default();
    stats.record_way_missing_nodes(3);
    stats.record_way_missing_nodes(2);
    stats.record_relation_missing_way_ref();
    stats.record_relation_missing_way_ref();
    stats.record_relation_with_missing_way_refs();
    stats.record_relation_non_way_member();
    stats.record_relation_nested_member();

    let snap = stats.snapshot();
    assert_eq!(snap.missing_way_node_refs, 5);
    assert_eq!(snap.ways_with_missing_node_refs, 2);
    assert_eq!(snap.missing_relation_way_refs, 2);
    assert_eq!(snap.relations_with_missing_way_refs, 1);
    assert_eq!(snap.relation_non_way_members, 1);
    assert_eq!(snap.relation_nested_members, 1);
}

#[test]
fn oversize_top_list_keeps_largest_tiles_sorted() {
    let mut top = [OversizeTile::default(); TILE_OVERSIZE_TOP_N];
    insert_top_oversized(
        &mut top,
        OversizeTile {
            tile_id: pmtiles_writer::xy_to_tile_id(1, 0, 0),
            bytes: 100,
        },
    );
    insert_top_oversized(
        &mut top,
        OversizeTile {
            tile_id: pmtiles_writer::xy_to_tile_id(1, 1, 0),
            bytes: 900,
        },
    );
    insert_top_oversized(
        &mut top,
        OversizeTile {
            tile_id: pmtiles_writer::xy_to_tile_id(1, 1, 1),
            bytes: 500,
        },
    );

    assert_eq!(top[0].bytes, 900);
    assert_eq!(top[1].bytes, 500);
    assert_eq!(top[2].bytes, 100);
}

#[test]
fn invalid_tile_ring_detected() {
    let bowtie = vec![(0, 0), (10, 10), (0, 10), (10, 0), (0, 0)];
    assert!(!is_valid_simple_tile_ring(&bowtie));
}

#[test]
fn invalid_merc_ring_detected_pre_quantization() {
    let bowtie = vec![
        Point { x: 0.3, y: 0.3 },
        Point { x: 0.7, y: 0.7 },
        Point { x: 0.3, y: 0.7 },
        Point { x: 0.7, y: 0.3 },
    ];
    assert!(!is_valid_simple_ring_points(&bowtie));
}

/// Helper: build a BoundaryLabels match with the given admin_level and default min_zoom=5.
fn boundary_labels_match(admin_level: i64) -> LayerMatch {
    LayerMatch {
        layer: Layer::BoundaryLabels,
        min_zoom: 5,
        max_zoom: 14,
        geom_expect: GeomExpect::PolygonPointOnSurface,
        attrs: smallvec![
            ("admin_level", AttrValue::Int(admin_level), 0),
            ("name", AttrValue::Str(Cow::Borrowed("TestCountry")), 0),
        ],
    }
}

/// admin_level=2 with area >= 2,000,000 km^2 -> min_zoom overridden to 2
#[test]
fn boundary_label_admin2_large_area() {
    let area_m2 = 2_000_000.0 * 1e6; // exactly 2M km^2
    let mut matches = vec![boundary_labels_match(2)];
    enrich_polygon_matches(&mut matches, area_m2);

    assert_eq!(matches[0].min_zoom, 2);
    // Also verify way_area was added (in hectares)
    let way_area_attr = matches[0].attrs.iter()
        .find(|(k, _, _)| *k == "way_area")
        .expect("way_area attr missing");
    if let AttrValue::Float(h) = way_area_attr.1 {
        let expected_hectares = area_m2 / 10_000.0;
        assert!((h - expected_hectares).abs() < 0.01, "way_area hectares mismatch");
    } else {
        panic!("way_area should be Float");
    }
}

/// admin_level=4 with area >= 700,000 km^2 -> min_zoom overridden to 3
#[test]
fn boundary_label_admin4_700k_km2() {
    let area_m2 = 700_000.0 * 1e6;
    let mut matches = vec![boundary_labels_match(4)];
    enrich_polygon_matches(&mut matches, area_m2);

    assert_eq!(matches[0].min_zoom, 3);
}

/// admin_level=4 with area >= 100,000 km^2 (but < 700,000) -> min_zoom overridden to 4
#[test]
fn boundary_label_admin4_100k_km2() {
    let area_m2 = 150_000.0 * 1e6; // 150k km^2
    let mut matches = vec![boundary_labels_match(4)];
    enrich_polygon_matches(&mut matches, area_m2);

    assert_eq!(matches[0].min_zoom, 4);
}

/// admin_level=4 with area < 100,000 km^2 -> min_zoom stays at default (5)
#[test]
fn boundary_label_admin4_small_area() {
    let area_m2 = 50_000.0 * 1e6; // 50k km^2 — below 100k threshold
    let mut matches = vec![boundary_labels_match(4)];
    enrich_polygon_matches(&mut matches, area_m2);

    assert_eq!(matches[0].min_zoom, 5, "small area should keep default min_zoom=5");
}

/// Non-BoundaryLabels layer should be completely unchanged by enrich_polygon_matches.
#[test]
fn non_boundary_labels_unchanged() {
    let mut matches = vec![LayerMatch {
        layer: Layer::Buildings,
        min_zoom: 14,
        max_zoom: 14,
        geom_expect: GeomExpect::Polygon,
        attrs: smallvec![],
    }];
    let original_min_zoom = matches[0].min_zoom;
    let original_attr_count = matches[0].attrs.len();

    enrich_polygon_matches(&mut matches, 9_999_999_999.0);

    assert_eq!(matches[0].min_zoom, original_min_zoom);
    assert_eq!(matches[0].attrs.len(), original_attr_count, "attrs should not be modified");
}

#[test]
fn block_shared_node_annotation_marks_only_shared_interiors() {
    let mut raw = vec![
        RawWay {
            way_id: 1,
            node_refs: vec![10, 20, 30],
            preserve_node_refs: Vec::new(),
            coords_e7: Vec::new(),
            tags: Vec::new(),
        },
        RawWay {
            way_id: 2,
            node_refs: vec![99, 20, 88],
            preserve_node_refs: Vec::new(),
            coords_e7: Vec::new(),
            tags: Vec::new(),
        },
        RawWay {
            way_id: 3,
            node_refs: vec![20, 777], // shared, but endpoint only => ignored
            preserve_node_refs: Vec::new(),
            coords_e7: Vec::new(),
            tags: Vec::new(),
        },
    ];

    annotate_block_shared_node_refs(&mut raw);

    assert_eq!(raw[0].preserve_node_refs, vec![20]);
    assert_eq!(raw[1].preserve_node_refs, vec![20]);
    assert!(raw[2].preserve_node_refs.is_empty());
}

#[test]
fn block_shared_node_annotation_marks_closed_ring_shared_vertices() {
    let mut raw = vec![
        RawWay {
            way_id: 10,
            node_refs: vec![1, 2, 3, 4, 1],
            preserve_node_refs: Vec::new(),
            coords_e7: Vec::new(),
            tags: Vec::new(),
        },
        RawWay {
            way_id: 11,
            node_refs: vec![3, 4, 5, 6, 3],
            preserve_node_refs: Vec::new(),
            coords_e7: Vec::new(),
            tags: Vec::new(),
        },
    ];

    annotate_block_shared_node_refs(&mut raw);

    assert_eq!(raw[0].preserve_node_refs, vec![3, 4]);
    assert_eq!(raw[1].preserve_node_refs, vec![3, 4]);
}

#[test]
fn relation_shared_vertex_keys_detects_shared_closed_way_vertices() {
    let member_ways = vec![
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 0.0, y: 0.0 },
                Point { x: 1.0, y: 0.0 },
                Point { x: 1.0, y: 1.0 },
                Point { x: 0.0, y: 1.0 },
                Point { x: 0.0, y: 0.0 },
            ],
        },
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 1.0, y: 0.0 },
                Point { x: 2.0, y: 0.0 },
                Point { x: 2.0, y: 1.0 },
                Point { x: 1.0, y: 1.0 },
                Point { x: 1.0, y: 0.0 },
            ],
        },
    ];
    let keys = relation_shared_vertex_keys(&member_ways);
    assert!(keys.contains(&merc_point_key(&Point { x: 1.0, y: 0.0 })));
    assert!(keys.contains(&merc_point_key(&Point { x: 1.0, y: 1.0 })));
    assert_eq!(keys.len(), 2);
}

#[test]
fn unwrap_antimeridian_path_keeps_crossing_segment_local() {
    let mut pts = vec![
        Point { x: 0.995, y: 0.4 },
        Point { x: 0.005, y: 0.4 },
    ];
    let changed = unwrap_antimeridian_path(&mut pts, false);
    assert!(changed);
    assert!(pts[1].x > 1.0, "second point should unwrap across +1 seam");
    assert!(
        (pts[1].x - pts[0].x).abs() < 0.05,
        "segment should stay short after unwrapping"
    );
}

#[test]
fn antimeridian_shifts_for_bbox_returns_wrap_shifts() {
    let bbox = MercBbox {
        min_x: 0.99,
        min_y: 0.1,
        max_x: 1.01,
        max_y: 0.2,
    };
    let shifts = antimeridian_shifts_for_bbox(&bbox);
    assert_eq!(shifts.len(), 2);
    assert!(shifts.contains(&0.0));
    assert!(shifts.contains(&-1.0));
}

#[test]
fn encode_tile_batch_rejects_unimplemented_mlt_format() {
    match encode_tile_batch(&[], 6, TilePayloadFormat::Mlt) {
        Ok(encoded) => assert!(encoded.is_empty(), "empty batches should remain empty"),
        Err(err) => panic!("empty mlt batch should not fail: {err}"),
    }
}

#[test]
fn encode_tile_batch_mvt_empty_batch_is_empty() {
    let encoded = encode_tile_batch(&[], 6, TilePayloadFormat::Mvt)
        .expect("mvt format should encode successfully");
    assert!(encoded.is_empty());
}

#[test]
fn encode_tile_batch_mlt_reports_tile_context() {
    let tile = PendingTile {
        tile_id: pmtiles_writer::xy_to_tile_id(3, 4, 5),
        features: Vec::new(),
    };
    match encode_tile_batch(&[tile], 6, TilePayloadFormat::Mlt) {
        Ok(_) => panic!("mlt format should fail for non-empty batch"),
        Err(err) => {
            let msg = err.to_string();
            assert!(msg.contains("mlt encode failed for tile 3/4/5"));
            assert!(msg.contains("mlt encoder is not implemented yet"));
        }
    }
}

// -----------------------------------------------------------------------
// Helpers for emit tests — decode SortRecord payloads
// -----------------------------------------------------------------------

use crate::sort;
use crate::mvt;

fn test_layer_match(layer: Layer, geom_expect: GeomExpect) -> LayerMatch {
    LayerMatch {
        layer,
        min_zoom: 0,
        max_zoom: 14,
        geom_expect,
        attrs: smallvec![("kind", AttrValue::Str(Cow::Borrowed("test")), 0)],
    }
}

/// Decode the sort key fields from a SortRecord.
fn decode_key(rec: &SortRecord) -> (u64, u8) {
    let tile_id = sort::tile_id_from_key(rec.key);
    let layer_idx = sort::layer_from_key(rec.key);
    (tile_id, layer_idx)
}

/// Decode the wire format header from a SortRecord's data payload.
/// Returns (osm_id, geom_type_byte, geom_cmd_count).
fn decode_data_header(data: &[u8]) -> (u64, u8, u32) {
    let osm_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let gt = data[8];
    let cmd_count = u16::from_le_bytes(data[9..11].try_into().unwrap());
    (osm_id, gt, u32::from(cmd_count))
}

/// Decode the attribute count from a SortRecord's data payload.
fn decode_attr_count(data: &[u8]) -> u8 {
    let cmd_count = u16::from_le_bytes(data[9..11].try_into().unwrap()) as usize;
    let attr_start = 11 + cmd_count * 4;
    data[attr_start]
}

/// Decode the full record via add_feature_to_layer and return the layer builder.
fn decode_to_layer(data: &[u8]) -> mvt::LayerBuilder {
    let mut lb = mvt::LayerBuilder::new("test");
    let mut gp = Vec::new();
    let mut tp = Vec::new();
    crate::wire_format::add_feature_to_layer(&mut lb, data, &mut gp, &mut tp);
    lb
}

// -----------------------------------------------------------------------
// emit_point_or_centroid tests (formerly emit_point_feature)
// -----------------------------------------------------------------------

#[test]
fn emit_point_empty_coords() {
    let m = test_layer_match(Layer::Pois, GeomExpect::Point);
    let bbox = MercBbox { min_x: 0.0, min_y: 0.0, max_x: 1.0, max_y: 1.0 };
    let mut records = Vec::new();
    let mut scratch = PointEmitScratch::new();
    let count = emit_point_or_centroid(1, &[], None, &bbox, &m, 0, 0, &mut records, &mut scratch);
    assert_eq!(count, 0);
    assert!(records.is_empty());
}

#[test]
fn emit_point_decodes_correctly() {
    let m = test_layer_match(Layer::Pois, GeomExpect::Point);
    let coords = [Point { x: 0.5, y: 0.5 }];
    let bbox = MercBbox { min_x: 0.0, min_y: 0.0, max_x: 1.0, max_y: 1.0 };
    let mut records = Vec::new();
    let mut scratch = PointEmitScratch::new();
    emit_point_or_centroid(42, &coords, None, &bbox, &m, 0, 0, &mut records, &mut scratch);
    assert_eq!(records.len(), 1);

    let rec = &records[0];

    // Sort key: tile_id for z=0/x=0/y=0, layer = Pois
    let (tile_id, layer_idx) = decode_key(rec);
    assert_eq!(tile_id, pmtiles_writer::xy_to_tile_id(0, 0, 0));
    assert_eq!(layer_idx, Layer::Pois as u8);

    // Wire format: osm_id=42, geom_type=Point(1), has geometry commands
    let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
    assert_eq!(osm_id, 42);
    assert_eq!(gt, 1); // Point
    assert!(cmd_count > 0, "point should have geometry commands");

    // Attributes: 1 attr ("kind" = "test")
    assert_eq!(decode_attr_count(&rec.data), 1);

    // Full decode roundtrip
    let lb = decode_to_layer(&rec.data);
    assert_eq!(lb.test_feature_count(), 1);
    let f = lb.test_feature(0);
    assert_eq!(f.id, Some(42));
    assert_eq!(f.geom_type, mvt::GeomType::Point);
    let (k0, v0) = f.tags[0];
    assert_eq!(lb.test_key(k0), "kind");
    assert_eq!(*lb.test_value(v0), mvt::Value::String("test".to_string()));
}

#[test]
fn emit_point_multi_zoom_tile_ids_differ() {
    let m = test_layer_match(Layer::Pois, GeomExpect::Point);
    // Use a point clearly inside one z1 tile (not on a boundary)
    let coords = [Point { x: 0.25, y: 0.25 }];
    let bbox = MercBbox { min_x: 0.25, min_y: 0.25, max_x: 0.25, max_y: 0.25 };
    let mut records = Vec::new();
    let mut scratch = PointEmitScratch::new();
    emit_point_or_centroid(7, &coords, None, &bbox, &m, 0, 1, &mut records, &mut scratch);

    // Should get 1 record at z=0 and 1 record at z=1 = 2 total
    assert_eq!(records.len(), 2);

    // The tile IDs should differ (z0 vs z1 are different Hilbert IDs)
    let (tid0, _) = decode_key(&records[0]);
    let (tid1, _) = decode_key(&records[1]);
    assert_ne!(tid0, tid1, "z0 and z1 tile IDs should differ");

    // Both should decode to the same osm_id
    let (id0, _, _) = decode_data_header(&records[0].data);
    let (id1, _, _) = decode_data_header(&records[1].data);
    assert_eq!(id0, 7);
    assert_eq!(id1, 7);
}

// -----------------------------------------------------------------------
// emit_line_feature tests
// -----------------------------------------------------------------------

#[test]
fn emit_line_too_few_points() {
    let m = test_layer_match(Layer::Streets, GeomExpect::Line);
    let coords = [Point { x: 0.5, y: 0.5 }];
    let mut records = Vec::new();
    let mut scratch = LineEmitScratch::new();
    let count = emit_line_feature(101, &coords, &[], &m, 0, 0, &mut records, &mut scratch);
    assert_eq!(count, 0);
    assert!(records.is_empty());
}

#[test]
fn emit_line_decodes_correctly() {
    let m = test_layer_match(Layer::Streets, GeomExpect::Line);
    let coords = [
        Point { x: 0.3, y: 0.3 },
        Point { x: 0.7, y: 0.7 },
    ];
    let mut records = Vec::new();
    let mut scratch = LineEmitScratch::new();
    emit_line_feature(100, &coords, &[], &m, 0, 0, &mut records, &mut scratch);
    assert_eq!(records.len(), 1);

    let rec = &records[0];

    // Sort key
    let (tile_id, layer_idx) = decode_key(rec);
    assert_eq!(tile_id, pmtiles_writer::xy_to_tile_id(0, 0, 0));
    assert_eq!(layer_idx, Layer::Streets as u8);

    // Wire format
    let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
    assert_eq!(osm_id, 100);
    assert_eq!(gt, 2); // LineString
    assert!(cmd_count >= 2, "linestring needs MoveTo + LineTo commands");

    // Full decode roundtrip
    let lb = decode_to_layer(&rec.data);
    let f = lb.test_feature(0);
    assert_eq!(f.id, Some(100));
    assert_eq!(f.geom_type, mvt::GeomType::LineString);
    assert_eq!(f.tags.len(), 1);
}

#[test]
fn emit_line_cascading_simplification() {
    // A line that should survive at z=14 but may get simplified away at low zoom.
    // At z=0, simplification tolerance is very large, so a short line may vanish.
    let m = test_layer_match(Layer::Streets, GeomExpect::Line);
    let coords = [
        Point { x: 0.500_000, y: 0.500_000 },
        Point { x: 0.500_001, y: 0.500_001 },
    ];
    let mut records = Vec::new();
    let mut scratch = LineEmitScratch::new();
    emit_line_feature(99, &coords, &[], &m, 0, 14, &mut records, &mut scratch);

    // At z=14 this line is ~0.4 pixel which is sub-pixel, but Streets skips
    // the size filter, so it should still produce a record at z=14.
    // At lower zooms, simplification may collapse it.
    let z14_records: Vec<_> = records.iter().filter(|r| {
        let tid = sort::tile_id_from_key(r.key);
        let (z, _, _) = pmtiles_writer::tile_id_to_zxy(tid);
        z == 14
    }).collect();
    assert!(!z14_records.is_empty(), "line should survive at z=14 for Streets layer");
}

// -----------------------------------------------------------------------
// emit_polygon_feature tests
// -----------------------------------------------------------------------

#[test]
fn emit_polygon_too_few_points() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let coords = [
        Point { x: 0.3, y: 0.3 },
        Point { x: 0.7, y: 0.3 },
        Point { x: 0.5, y: 0.7 },
    ];
    let mut records = Vec::new();
    let mut scratch = PolygonEmitScratch::new();
    let count = emit_polygon_feature(201, &coords, &[], &m, 0, 0, &mut records, &mut scratch);
    assert_eq!(count, 0);
    assert!(records.is_empty());
}

#[test]
fn emit_polygon_decodes_correctly() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let coords = [
        Point { x: 0.3, y: 0.3 },
        Point { x: 0.7, y: 0.3 },
        Point { x: 0.7, y: 0.7 },
        Point { x: 0.3, y: 0.7 },
        Point { x: 0.3, y: 0.3 },
    ];
    let mut records = Vec::new();
    let mut scratch = PolygonEmitScratch::new();
    emit_polygon_feature(200, &coords, &[], &m, 0, 0, &mut records, &mut scratch);
    assert_eq!(records.len(), 1);

    let rec = &records[0];

    // Sort key
    let (tile_id, layer_idx) = decode_key(rec);
    assert_eq!(tile_id, pmtiles_writer::xy_to_tile_id(0, 0, 0));
    assert_eq!(layer_idx, Layer::Buildings as u8);

    // Wire format
    let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
    assert_eq!(osm_id, 200);
    assert_eq!(gt, 3); // Polygon
    // A polygon ring needs MoveTo + LineTo(n-1) + ClosePath = at least 3 commands
    assert!(cmd_count >= 3, "polygon should have MoveTo + LineTo + ClosePath");

    // Attributes
    assert_eq!(decode_attr_count(&rec.data), 1);

    // Full decode roundtrip
    let lb = decode_to_layer(&rec.data);
    let f = lb.test_feature(0);
    assert_eq!(f.id, Some(200));
    assert_eq!(f.geom_type, mvt::GeomType::Polygon);
    let (k0, v0) = f.tags[0];
    assert_eq!(lb.test_key(k0), "kind");
    assert_eq!(*lb.test_value(v0), mvt::Value::String("test".to_string()));
}

#[test]
fn emit_polygon_zoom_dependent_attrs() {
    // Attribute with min_zoom=10 should only appear at z>=10.
    // Use a tiny polygon that fits in a single tile at each test zoom.
    let m_z0 = LayerMatch {
        layer: Layer::Buildings,
        min_zoom: 0,
        max_zoom: 14,
        geom_expect: GeomExpect::Polygon,
        attrs: smallvec![
            ("kind", AttrValue::Str(Cow::Borrowed("building")), 0),
            ("height", AttrValue::Float(15.0), 10),
        ],
    };
    let coords_z0 = [
        Point { x: 0.3, y: 0.3 },
        Point { x: 0.7, y: 0.3 },
        Point { x: 0.7, y: 0.7 },
        Point { x: 0.3, y: 0.7 },
        Point { x: 0.3, y: 0.3 },
    ];
    // At z=0: only 1 attr ("kind", min_zoom=0)
    let mut records = Vec::new();
    let mut scratch = PolygonEmitScratch::new();
    emit_polygon_feature(300, &coords_z0, &[], &m_z0, 0, 0, &mut records, &mut scratch);
    assert_eq!(records.len(), 1);
    let lb = decode_to_layer(&records[0].data);
    let f = lb.test_feature(0);
    assert_eq!(f.tags.len(), 1, "at z=0 only the always-on attr should be present");

    // At z=14: both attrs. Use a tiny polygon inside one z=14 tile.
    let m_z14 = LayerMatch {
        layer: Layer::Buildings,
        min_zoom: 14,
        max_zoom: 14,
        geom_expect: GeomExpect::Polygon,
        attrs: smallvec![
            ("kind", AttrValue::Str(Cow::Borrowed("building")), 0),
            ("height", AttrValue::Float(15.0), 10),
        ],
    };
    let coords_z14 = [
        Point { x: 0.500_00, y: 0.500_00 },
        Point { x: 0.500_05, y: 0.500_00 },
        Point { x: 0.500_05, y: 0.500_05 },
        Point { x: 0.500_00, y: 0.500_05 },
        Point { x: 0.500_00, y: 0.500_00 },
    ];
    records.clear();
    emit_polygon_feature(300, &coords_z14, &[], &m_z14, 14, 14, &mut records, &mut scratch);
    assert_eq!(records.len(), 1);
    let lb = decode_to_layer(&records[0].data);
    let f = lb.test_feature(0);
    assert_eq!(f.tags.len(), 2, "at z=14 both attrs should be present");
    let (k1, v1) = f.tags[1];
    assert_eq!(lb.test_key(k1), "height");
    assert_eq!(*lb.test_value(v1), mvt::Value::Double(15.0));
}

#[test]
fn emit_polygon_skips_self_intersecting_ring_below_z14() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    // Bow-tie ring (self-intersecting).
    let coords = [
        Point { x: 0.3, y: 0.3 },
        Point { x: 0.7, y: 0.7 },
        Point { x: 0.3, y: 0.7 },
        Point { x: 0.7, y: 0.3 },
        Point { x: 0.3, y: 0.3 },
    ];

    let mut records = Vec::new();
    let mut scratch = PolygonEmitScratch::new();
    let count = emit_polygon_feature(500, &coords, &[], &m, 0, 0, &mut records, &mut scratch);
    assert_eq!(count, 0);
    assert!(records.is_empty());
}

#[test]
fn emit_polygon_preserve_mask_keeps_required_vertices() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let coords = [
        Point { x: 0.1, y: 0.1 },
        Point { x: 0.3, y: 0.10001 },
        Point { x: 0.5, y: 0.10002 }, // pin this vertex
        Point { x: 0.7, y: 0.10001 },
        Point { x: 0.9, y: 0.1 },
        Point { x: 0.9, y: 0.9 },
        Point { x: 0.1, y: 0.9 },
        Point { x: 0.1, y: 0.1 },
    ];

    let mut records_plain = Vec::new();
    let mut records_pinned = Vec::new();
    let mut scratch_plain = PolygonEmitScratch::new();
    let mut scratch_pinned = PolygonEmitScratch::new();
    emit_polygon_feature(
        900,
        &coords,
        &[false; 8],
        &m,
        0,
        0,
        &mut records_plain,
        &mut scratch_plain,
    );
    emit_polygon_feature(
        900,
        &coords,
        &[false, false, true, false, false, false, false, false],
        &m,
        0,
        0,
        &mut records_pinned,
        &mut scratch_pinned,
    );

    assert_eq!(records_plain.len(), 1);
    assert_eq!(records_pinned.len(), 1);

    let (_, _, plain_cmd_count) = decode_data_header(&records_plain[0].data);
    let (_, _, pinned_cmd_count) = decode_data_header(&records_pinned[0].data);
    assert!(
        pinned_cmd_count > plain_cmd_count,
        "pinned polygon vertex should increase retained geometry detail"
    );
}

#[test]
fn emit_multipolygon_preserve_keys_keep_required_vertices() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        Point { x: 0.1, y: 0.1 },
        Point { x: 0.3, y: 0.10001 },
        Point { x: 0.5, y: 0.10002 }, // keep
        Point { x: 0.7, y: 0.10001 },
        Point { x: 0.9, y: 0.1 },
        Point { x: 0.9, y: 0.9 },
        Point { x: 0.1, y: 0.9 },
        Point { x: 0.1, y: 0.1 },
    ];
    let inners: Vec<Vec<Point>> = Vec::new();
    let mut keys = rustc_hash::FxHashSet::default();
    keys.insert(merc_point_key(&Point { x: 0.5, y: 0.10002 }));

    let mut records_plain = Vec::new();
    let mut records_pinned = Vec::new();
    let mut emit_plain = MultipolygonEmitScratch::new();
    let mut emit_pinned = MultipolygonEmitScratch::new();
    let mut simp_plain = geometry::SimplifyMultiScratch::new();
    let mut simp_pinned = geometry::SimplifyMultiScratch::new();

    emit_multipolygon_feature(
        990,
        &outer,
        &inners,
        None,
        &m,
        0,
        0,
        &mut records_plain,
        &mut emit_plain,
        &mut simp_plain,
    );
    emit_multipolygon_feature(
        990,
        &outer,
        &inners,
        Some(&keys),
        &m,
        0,
        0,
        &mut records_pinned,
        &mut emit_pinned,
        &mut simp_pinned,
    );

    assert_eq!(records_plain.len(), 1);
    assert_eq!(records_pinned.len(), 1);
    let (_, _, plain_cmd_count) = decode_data_header(&records_plain[0].data);
    let (_, _, pinned_cmd_count) = decode_data_header(&records_pinned[0].data);
    assert!(pinned_cmd_count > plain_cmd_count);
}

// -----------------------------------------------------------------------
// emit_multipolygon_feature tests
// -----------------------------------------------------------------------

#[test]
fn emit_multipolygon_empty_outer() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let mut records = Vec::new();
    let mut emit_scratch = MultipolygonEmitScratch::new();
    let mut simp_scratch = geometry::SimplifyMultiScratch::new();

    let count = emit_multipolygon_feature(
        401,
        &[],
        &[],
        None,
        &m,
        0,
        0,
        &mut records,
        &mut emit_scratch,
        &mut simp_scratch,
    );

    assert_eq!(count, 0);
    assert!(records.is_empty());
}

#[test]
fn emit_multipolygon_with_hole_decodes_correctly() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        Point { x: 0.20, y: 0.20 },
        Point { x: 0.80, y: 0.20 },
        Point { x: 0.80, y: 0.80 },
        Point { x: 0.20, y: 0.80 },
        Point { x: 0.20, y: 0.20 },
    ];
    let inners = vec![vec![
        Point { x: 0.40, y: 0.40 },
        Point { x: 0.60, y: 0.40 },
        Point { x: 0.60, y: 0.60 },
        Point { x: 0.40, y: 0.60 },
        Point { x: 0.40, y: 0.40 },
    ]];

    let mut records = Vec::new();
    let mut emit_scratch = MultipolygonEmitScratch::new();
    let mut simp_scratch = geometry::SimplifyMultiScratch::new();
    let count = emit_multipolygon_feature(
        402,
        &outer,
        &inners,
        None,
        &m,
        0,
        0,
        &mut records,
        &mut emit_scratch,
        &mut simp_scratch,
    );

    assert_eq!(count, 1);
    assert_eq!(records.len(), 1);

    let rec = &records[0];
    let (tile_id, layer_idx) = decode_key(rec);
    assert_eq!(tile_id, pmtiles_writer::xy_to_tile_id(0, 0, 0));
    assert_eq!(layer_idx, Layer::Buildings as u8);

    let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
    assert_eq!(osm_id, 402);
    assert_eq!(gt, 3);
    assert!(cmd_count >= 6, "multipolygon with hole should encode multiple ring commands");

    let lb = decode_to_layer(&rec.data);
    assert_eq!(lb.test_feature_count(), 1);
    let f = lb.test_feature(0);
    assert_eq!(f.id, Some(402));
    assert_eq!(f.geom_type, mvt::GeomType::Polygon);
    assert_eq!(f.tags.len(), 1);
}

#[test]
fn emit_multipolygon_large_shape_clips_to_multiple_tiles() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        Point { x: 0.10, y: 0.10 },
        Point { x: 0.90, y: 0.10 },
        Point { x: 0.90, y: 0.90 },
        Point { x: 0.10, y: 0.90 },
        Point { x: 0.10, y: 0.10 },
    ];
    let inners = vec![vec![
        Point { x: 0.45, y: 0.45 },
        Point { x: 0.55, y: 0.45 },
        Point { x: 0.55, y: 0.55 },
        Point { x: 0.45, y: 0.55 },
        Point { x: 0.45, y: 0.45 },
    ]];

    let mut records = Vec::new();
    let mut emit_scratch = MultipolygonEmitScratch::new();
    let mut simp_scratch = geometry::SimplifyMultiScratch::new();
    let count = emit_multipolygon_feature(
        403,
        &outer,
        &inners,
        None,
        &m,
        2,
        2,
        &mut records,
        &mut emit_scratch,
        &mut simp_scratch,
    );

    assert_eq!(usize::try_from(count).unwrap(), records.len());
    assert!(records.len() > 1, "large multipolygon should clip into multiple z2 tiles");

    for rec in &records {
        let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
        assert_eq!(osm_id, 403);
        assert_eq!(gt, 3);
        assert!(cmd_count > 0);
    }
}

// ---------------------------------------------------------------------------
// Checkpoint roundtrip tests
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_roundtrip() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let bounds = geometry::MercBbox {
        min_x: 0.1,
        min_y: 0.2,
        max_x: 0.8,
        max_y: 0.9,
    };
    save_checkpoint(dir.path(), &bounds, 42).unwrap();
    let (loaded_bounds, loaded_chunks) = load_checkpoint(dir.path()).unwrap();
    assert!((loaded_bounds.min_x - 0.1).abs() < 1e-10);
    assert!((loaded_bounds.min_y - 0.2).abs() < 1e-10);
    assert!((loaded_bounds.max_x - 0.8).abs() < 1e-10);
    assert!((loaded_bounds.max_y - 0.9).abs() < 1e-10);
    assert_eq!(loaded_chunks, 42);
}

#[test]
fn sort_chunk_count_roundtrip() {
    let dir = tempfile::tempdir().expect("create tempdir");
    save_sort_chunk_count(dir.path(), Some(17)).unwrap();
    let loaded = load_sort_chunk_count(dir.path());
    assert_eq!(loaded, Some(17));
}

#[test]
fn sort_chunk_count_none_no_file() {
    let dir = tempfile::tempdir().expect("create tempdir");
    save_sort_chunk_count(dir.path(), None).unwrap();
    let loaded = load_sort_chunk_count(dir.path());
    assert_eq!(loaded, None);
}

#[test]
fn land_mask_checkpoint_roundtrip() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let mask = geometry::LandMask::new();
    // Mark a small bbox that covers a known z14 tile.
    let bbox = geometry::MercBbox {
        min_x: 0.5,
        max_x: 0.500_1,
        min_y: 0.5,
        max_y: 0.500_1,
    };
    mask.mark_bbox(&bbox);
    assert!(mask.has_land(14, 8192, 8192));
    save_land_mask(dir.path(), &mask).unwrap();
    let loaded = load_land_mask(dir.path()).expect("should load");
    assert!(loaded.has_land(14, 8192, 8192));
    assert!(!loaded.has_land(14, 0, 0));
}

#[test]
fn load_checkpoint_missing_file() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let result = load_checkpoint(dir.path());
    assert!(result.is_err());
}

#[test]
fn load_land_mask_missing_returns_none() {
    let dir = tempfile::tempdir().expect("create tempdir");
    assert!(load_land_mask(dir.path()).is_none());
}
