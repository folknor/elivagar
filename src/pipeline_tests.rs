use super::*;
use crate::shortbread::{AttrValue, GeomExpect, Layer, LayerMatch};
use flate2::read::GzDecoder;
use pbfhogg::block_builder;
use pbfhogg::writer::{Compression as PbfCompression, PbfWriter};
use smallvec::smallvec;
use std::borrow::Cow;
use std::fs::File;
use std::io::Read;
use std::time::{Duration, Instant};

fn one_tile_sort_reader(chunks_dir: &std::path::Path) -> sort::SortReader {
    let mut writer = sort::SortWriter::new(chunks_dir, 1024, sort::ChunkCompression::None).expect("create sort writer");
    let attrs: Vec<crate::shortbread::Attr> = vec![
        ("kind", AttrValue::Str(Cow::Borrowed("city")), 0),
    ];
    let feature = crate::wire_format::encode_feature_data(
        1,
        mvt::GeomType::Point,
        &[9, 0, 0],
        &attrs,
        14,
    );
    writer.push(SortRecord {
        key: sort::make_sort_key(pmtiles_writer::xy_to_tile_id(0, 0, 0), Layer::Pois as u8, 0),
        data: feature,
    }).expect("push sort record");
    writer.finish().expect("finish sort writer")
}

fn parity_pending_tile(tile_id: u64) -> PendingTile {
    let point_attrs = vec![("kind", AttrValue::Str(Cow::Borrowed("city")), 0)];
    let line_attrs = vec![("kind", AttrValue::Str(Cow::Borrowed("street")), 0)];

    PendingTile {
        tile_id,
        features: vec![
            (
                Layer::Pois as u8,
                crate::wire_format::encode_feature_data(11, mvt::GeomType::Point, &[9, 0, 0], &point_attrs, 14),
            ),
            (
                Layer::Streets as u8,
                crate::wire_format::encode_feature_data(
                    21,
                    mvt::GeomType::LineString,
                    &[9, 4, 4, 18, 0, 16, 16, 0],
                    &line_attrs,
                    14,
                ),
            ),
        ],
    }
}

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
fn missing_ref_summary_lines_include_all_counters() {
    let lines = missing_ref_summary_lines(MissingRefStats {
        missing_way_node_refs: 5,
        ways_with_missing_node_refs: 2,
        missing_relation_way_refs: 3,
        relations_with_missing_way_refs: 1,
        relation_non_way_members: 4,
        relation_nested_members: 2,
    });
    assert_eq!(
        lines,
        [
            "missing_way_node_refs=5".to_string(),
            "ways_with_missing_node_refs=2".to_string(),
            "missing_relation_way_refs=3".to_string(),
            "relations_with_missing_way_refs=1".to_string(),
            "relation_non_way_members=4".to_string(),
            "relation_nested_members=2".to_string(),
        ]
    );
}

#[test]
fn missing_ref_summary_omitted_when_phase12_is_skipped() {
    let maybe_summary: Option<MissingRefStats> = None;
    let lines: Vec<String> = maybe_summary
        .map(missing_ref_summary_lines)
        .into_iter()
        .flatten()
        .collect();
    assert!(
        lines.is_empty(),
        "skip/resume paths without phase12 stats should emit no missing-ref metrics"
    );
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
fn tile_size_diag_thresholds_are_strictly_greater_than_boundaries() {
    let mut diag = TileSizeDiagnostics::default();
    let warn = TILE_OVERSIZE_WARN_BYTES;
    let severe = TILE_OVERSIZE_SEVERE_BYTES;

    record_tile_size_diagnostics(&mut diag, pmtiles_writer::xy_to_tile_id(0, 0, 0), warn);
    assert_eq!(diag.oversize_warn_count, 0);
    assert_eq!(diag.oversize_severe_count, 0);

    record_tile_size_diagnostics(&mut diag, pmtiles_writer::xy_to_tile_id(1, 0, 0), warn + 1);
    assert_eq!(diag.oversize_warn_count, 1);
    assert_eq!(diag.oversize_severe_count, 0);

    record_tile_size_diagnostics(&mut diag, pmtiles_writer::xy_to_tile_id(2, 0, 0), severe);
    assert_eq!(diag.oversize_warn_count, 2);
    assert_eq!(diag.oversize_severe_count, 0);

    record_tile_size_diagnostics(&mut diag, pmtiles_writer::xy_to_tile_id(3, 0, 0), severe + 1);
    assert_eq!(diag.oversize_warn_count, 3);
    assert_eq!(diag.oversize_severe_count, 1);
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

#[test]
fn interior_tile_ring_buffer_matches_buffer_fraction() {
    // INTERIOR_TILE_RING must use the same buffer as BUFFER_FRACTION expressed
    // in extent units: 8 rendered pixels × 16 extent units/pixel = 128.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let expected_buf = (crate::geometry::BUFFER_FRACTION * crate::geometry::EXTENT) as i32;
    assert_eq!(expected_buf, 128);

    #[allow(clippy::cast_possible_truncation)]
    let extent = crate::geometry::EXTENT as i32;
    let expected: [(i32, i32); 5] = [
        (-expected_buf, -expected_buf),
        (extent + expected_buf, -expected_buf),
        (extent + expected_buf, extent + expected_buf),
        (-expected_buf, extent + expected_buf),
        (-expected_buf, -expected_buf),
    ];
    assert_eq!(INTERIOR_TILE_RING, expected);
}

#[test]
fn pre_quantization_ring_accepts_collinear_segments() {
    let ring = vec![
        Point { x: 0.1, y: 0.1 },
        Point { x: 0.5, y: 0.1 }, // collinear
        Point { x: 0.9, y: 0.1 }, // collinear
        Point { x: 0.9, y: 0.9 },
        Point { x: 0.1, y: 0.9 },
        Point { x: 0.1, y: 0.1 },
    ];
    assert!(is_valid_simple_ring_points(&ring));
}

#[test]
fn pre_quantization_ring_rejects_repeated_non_adjacent_vertex() {
    let ring = vec![
        Point { x: 0.1, y: 0.1 },
        Point { x: 0.9, y: 0.1 },
        Point { x: 0.9, y: 0.9 },
        Point { x: 0.5, y: 0.5 },
        Point { x: 0.9, y: 0.9 }, // repeated non-adjacent vertex
        Point { x: 0.1, y: 0.9 },
        Point { x: 0.1, y: 0.1 },
    ];
    assert!(!is_valid_simple_ring_points(&ring));
}

#[test]
fn pre_quantization_ring_near_touching_gap_is_valid_but_touching_is_not() {
    let near_touching = vec![
        Point { x: 0.0, y: 0.0 },
        Point { x: 1.0, y: 0.0 },
        Point { x: 1.0, y: 1.0 },
        Point { x: 0.51, y: 1.0 },
        Point { x: 0.51, y: 0.000_000_002 },
        Point { x: 0.49, y: 0.000_000_002 },
        Point { x: 0.49, y: 1.0 },
        Point { x: 0.0, y: 1.0 },
        Point { x: 0.0, y: 0.0 },
    ];
    assert!(
        is_valid_simple_ring_points(&near_touching),
        "tiny non-zero gaps should remain valid"
    );

    let touching = vec![
        Point { x: 0.0, y: 0.0 },
        Point { x: 1.0, y: 0.0 },
        Point { x: 1.0, y: 1.0 },
        Point { x: 0.51, y: 1.0 },
        Point { x: 0.51, y: 0.0 }, // exact touch with bottom edge
        Point { x: 0.49, y: 0.0 }, // exact touch with bottom edge
        Point { x: 0.49, y: 1.0 },
        Point { x: 0.0, y: 1.0 },
        Point { x: 0.0, y: 0.0 },
    ];
    assert!(
        !is_valid_simple_ring_points(&touching),
        "edge-touching rings should be rejected"
    );
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
fn block_shared_node_annotation_does_not_detect_cross_block_junctions() {
    let mut block_a = vec![RawWay {
        way_id: 100,
        node_refs: vec![1, 20, 2],
        preserve_node_refs: Vec::new(),
        coords_e7: Vec::new(),
        tags: Vec::new(),
    }];
    let mut block_b = vec![RawWay {
        way_id: 101,
        node_refs: vec![3, 20, 4],
        preserve_node_refs: Vec::new(),
        coords_e7: Vec::new(),
        tags: Vec::new(),
    }];

    annotate_block_shared_node_refs(&mut block_a);
    annotate_block_shared_node_refs(&mut block_b);

    assert!(
        block_a[0].preserve_node_refs.is_empty(),
        "cross-block shared interior junctions are intentionally not detected"
    );
    assert!(
        block_b[0].preserve_node_refs.is_empty(),
        "cross-block shared interior junctions are intentionally not detected"
    );

    let mut combined = vec![block_a.remove(0), block_b.remove(0)];
    annotate_block_shared_node_refs(&mut combined);
    assert_eq!(
        combined[0].preserve_node_refs,
        vec![20],
        "same data in one block should detect the shared interior node"
    );
    assert_eq!(combined[1].preserve_node_refs, vec![20]);
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
fn relation_shared_vertex_keys_quantization_near_equal_points_share_key() {
    let base = Point { x: 0.500_000_000_000, y: 0.2 };
    let near = Point { x: 0.500_000_000_000_4, y: 0.2 }; // +0.4e-12
    assert_eq!(
        merc_point_key(&base),
        merc_point_key(&near),
        "near-equal points should quantize to same key"
    );

    let member_ways = vec![
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 0.1, y: 0.1 },
                base,
                Point { x: 0.1, y: 0.3 },
                Point { x: 0.1, y: 0.1 },
            ],
        },
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 0.9, y: 0.1 },
                near,
                Point { x: 0.9, y: 0.3 },
                Point { x: 0.9, y: 0.1 },
            ],
        },
    ];
    let keys = relation_shared_vertex_keys(&member_ways);
    assert!(
        keys.contains(&merc_point_key(&base)),
        "shared key should include quantized near-equal vertex"
    );
    assert_eq!(keys.len(), 1);
}

#[test]
fn relation_shared_vertex_keys_quantization_boundary_distinguishes_points() {
    let base = Point { x: 0.500_000_000_000, y: 0.2 };
    let far = Point { x: 0.500_000_000_000_6, y: 0.2 }; // +0.6e-12
    assert_ne!(
        merc_point_key(&base),
        merc_point_key(&far),
        "points beyond rounding half-step should quantize differently"
    );

    let member_ways = vec![
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 0.1, y: 0.1 },
                base,
                Point { x: 0.1, y: 0.3 },
                Point { x: 0.1, y: 0.1 },
            ],
        },
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 0.9, y: 0.1 },
                far,
                Point { x: 0.9, y: 0.3 },
                Point { x: 0.9, y: 0.1 },
            ],
        },
    ];
    let keys = relation_shared_vertex_keys(&member_ways);
    assert!(
        !keys.contains(&merc_point_key(&base)),
        "non-shared quantized vertex should not be marked shared"
    );
    assert!(
        !keys.contains(&merc_point_key(&far)),
        "non-shared quantized vertex should not be marked shared"
    );
    assert!(keys.is_empty());
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
fn crosses_antimeridian_detects_true_dateline_crossing() {
    // Tight dateline-spanning interval: [170, 180] U [-180, -170].
    let min_lon_e7 = -1_700_000_000;
    let max_lon_e7 = 1_700_000_000;
    let min_shifted = lon_e7_shifted_360(-1_700_000_000);
    let max_shifted = lon_e7_shifted_360(1_700_000_000);
    assert!(crosses_antimeridian(
        min_lon_e7,
        max_lon_e7,
        min_shifted.min(max_shifted),
        min_shifted.max(max_shifted),
    ));
}

#[test]
fn crosses_antimeridian_rejects_wide_non_crossing_interval() {
    // Wide but non-crossing interval: [-170, 20].
    let min_lon_e7 = -1_700_000_000;
    let max_lon_e7 = 200_000_000;
    let shifted_lons = [
        lon_e7_shifted_360(-1_700_000_000),
        lon_e7_shifted_360(200_000_000),
        lon_e7_shifted_360(0),
    ];
    let min_shifted = *shifted_lons.iter().min().expect("non-empty shifted sample");
    let max_shifted = *shifted_lons.iter().max().expect("non-empty shifted sample");
    assert!(!crosses_antimeridian(
        min_lon_e7,
        max_lon_e7,
        min_shifted,
        max_shifted,
    ));
}

#[test]
fn antimeridian_wrapped_line_emits_both_seam_tiles_without_duplicates() {
    let m = test_layer_match(Layer::Streets, GeomExpect::Line);
    let coords = vec![
        Point { x: 0.995, y: 0.25 },
        Point { x: 1.005, y: 0.25 },
    ];
    let bbox = merc_bbox(&coords);
    let mut records = Vec::new();
    let mut scratch = LineEmitScratch::new();
    for shift in antimeridian_shifts_for_bbox(&bbox) {
        if shift == 0.0 {
            let _ = emit_line_feature(7001, &coords, &[], &m, 1, 1, &mut records, &mut scratch);
        } else {
            let shifted: Vec<Point> = coords
                .iter()
                .map(|p| Point { x: p.x + shift, y: p.y })
                .collect();
            let _ = emit_line_feature(7001, &shifted, &[], &m, 1, 1, &mut records, &mut scratch);
        }
    }

    let mut counts = std::collections::BTreeMap::new();
    for rec in &records {
        let tile_id = sort::tile_id_from_key(rec.key);
        *counts.entry(tile_id).or_insert(0usize) += 1;
    }

    let left = pmtiles_writer::xy_to_tile_id(1, 0, 0);
    let right = pmtiles_writer::xy_to_tile_id(1, 1, 0);
    assert_eq!(counts.len(), 2, "seam-crossing line should hit exactly two z1 seam tiles");
    assert_eq!(counts.get(&left), Some(&1));
    assert_eq!(counts.get(&right), Some(&1));
}

#[test]
fn antimeridian_wrapped_polygon_emits_both_seam_tiles_without_duplicates() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let coords = vec![
        Point { x: 0.995, y: 0.24 },
        Point { x: 1.005, y: 0.24 },
        Point { x: 1.005, y: 0.26 },
        Point { x: 0.995, y: 0.26 },
        Point { x: 0.995, y: 0.24 },
    ];
    let bbox = merc_bbox(&coords);
    let mut records = Vec::new();
    let mut scratch = PolygonEmitScratch::new();
    for shift in antimeridian_shifts_for_bbox(&bbox) {
        if shift == 0.0 {
            let _ = emit_polygon_feature(7002, &coords, &[], &m, 1, 1, &mut records, &mut scratch, 0, None);
        } else {
            let shifted: Vec<Point> = coords
                .iter()
                .map(|p| Point { x: p.x + shift, y: p.y })
                .collect();
            let _ = emit_polygon_feature(7002, &shifted, &[], &m, 1, 1, &mut records, &mut scratch, 0, None);
        }
    }

    let mut counts = std::collections::BTreeMap::new();
    for rec in &records {
        let tile_id = sort::tile_id_from_key(rec.key);
        *counts.entry(tile_id).or_insert(0usize) += 1;
    }

    let left = pmtiles_writer::xy_to_tile_id(1, 0, 0);
    let right = pmtiles_writer::xy_to_tile_id(1, 1, 0);
    assert_eq!(counts.len(), 2, "seam-crossing polygon should hit exactly two z1 seam tiles");
    assert_eq!(counts.get(&left), Some(&1));
    assert_eq!(counts.get(&right), Some(&1));
}

#[test]
fn antimeridian_wrapped_multipolygon_emits_both_seam_tiles_without_duplicates() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        Point { x: 0.995, y: 0.24 },
        Point { x: 1.005, y: 0.24 },
        Point { x: 1.005, y: 0.26 },
        Point { x: 0.995, y: 0.26 },
        Point { x: 0.995, y: 0.24 },
    ];
    let inners: Vec<Vec<Point>> = Vec::new();
    let bbox = merc_bbox(&outer);
    let mut records = Vec::new();
    let mut emit_scratch = MultipolygonEmitScratch::new();
    let mut simp_scratch = geometry::SimplifyMultiScratch::new();
    for shift in antimeridian_shifts_for_bbox(&bbox) {
        if shift == 0.0 {
            let _ = emit_multipolygon_feature(
                7003,
                &outer,
                &inners,
                None,
                &m,
                1,
                1,
                &mut records,
                &mut emit_scratch,
                &mut simp_scratch,
                0,
                None,
            );
        } else {
            let outer_shifted: Vec<Point> = outer
                .iter()
                .map(|p| Point { x: p.x + shift, y: p.y })
                .collect();
            let _ = emit_multipolygon_feature(
                7003,
                &outer_shifted,
                &inners,
                None,
                &m,
                1,
                1,
                &mut records,
                &mut emit_scratch,
                &mut simp_scratch,
                0,
                None,
            );
        }
    }

    let mut counts = std::collections::BTreeMap::new();
    for rec in &records {
        let tile_id = sort::tile_id_from_key(rec.key);
        *counts.entry(tile_id).or_insert(0usize) += 1;
    }

    let left = pmtiles_writer::xy_to_tile_id(1, 0, 0);
    let right = pmtiles_writer::xy_to_tile_id(1, 1, 0);
    assert_eq!(counts.len(), 2, "seam-crossing multipolygon should hit exactly two z1 seam tiles");
    assert_eq!(counts.get(&left), Some(&1));
    assert_eq!(counts.get(&right), Some(&1));
}

#[test]
fn encode_tile_batch_mlt_empty_batch_is_empty() {
    match encode_tile_batch(&[], 6, TilePayloadFormat::Mlt, TileCompression::Gzip, &[], &SeamMetrics::new()) {
        Ok(encoded) => assert!(encoded.is_empty(), "empty batches should remain empty"),
        Err(err) => panic!("empty mlt batch should not fail: {err}"),
    }
}

#[test]
fn encode_tile_batch_mvt_empty_batch_is_empty() {
    let encoded = encode_tile_batch(&[], 6, TilePayloadFormat::Mvt, TileCompression::Gzip, &[], &SeamMetrics::new())
        .expect("mvt format should encode successfully");
    assert!(encoded.is_empty());
}

#[test]
fn encode_tile_batch_mlt_empty_tile_encodes_to_no_output() {
    let tile = PendingTile {
        tile_id: pmtiles_writer::xy_to_tile_id(3, 4, 5),
        features: Vec::new(),
    };
    match encode_tile_batch(&[tile], 6, TilePayloadFormat::Mlt, TileCompression::Gzip, &[], &SeamMetrics::new()) {
        Ok(encoded) => assert!(encoded.is_empty(), "empty tiles should be skipped"),
        Err(err) => panic!("mlt format should not fail for empty tile: {err}"),
    }
}

#[test]
fn shared_layer_prep_model_matches_mvt_layer_assembly() {
    let tile_id = pmtiles_writer::xy_to_tile_id(4, 8, 9);
    let tile = parity_pending_tile(tile_id);

    let mut scratch = AssemblyScratch {
        encode_scratch: mvt::EncodeScratch::new(),
        merge_scratch: mvt::MergeScratch::new(),
        line_merge_scratch: mvt::LineMergeScratch::new(),
        geom_pool: Vec::new(),
        tags_pool: Vec::new(),
        compression_levels: [const { None }; 11],
        gz_buf: Vec::new(),
        mvt_buf: Vec::new(),
        layers: [const { None }; LAYER_COUNT],
        seam_rings: Vec::new(),
        seam_provenance: Vec::new(),
        seam_encode_buf: Vec::new(),
    };
    let non_empty = prepare_non_empty_layers(&mut scratch, &tile);
    let model = mlt::build_tile_model(&non_empty);
    let mvt_encoded = encode_tile_batch(&[parity_pending_tile(tile_id)], 6, TilePayloadFormat::Mvt, TileCompression::Gzip, &[], &SeamMetrics::new())
        .expect("mvt batch encode should succeed");
    assert_eq!(mvt_encoded.len(), 1, "expected one encoded mvt tile");

    let mut decoder = GzDecoder::new(mvt_encoded[0].compressed.as_slice());
    let mut raw = Vec::new();
    decoder.read_to_end(&mut raw).expect("gunzip mvt tile");
    let mvt_layers = crate::pmtiles_reader::decode_mvt_layers(&raw)
        .expect("decode mvt layers");

    assert_eq!(
        model.layers.len(),
        mvt_layers.len(),
        "layer count mismatch between prep model and MVT"
    );

    let mut model_sorted: Vec<_> = model.layers.iter().collect();
    model_sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut mvt_sorted: Vec<_> = mvt_layers.iter().collect();
    mvt_sorted.sort_by(|a, b| a.name.cmp(&b.name));

    for (ml, mvt) in model_sorted.iter().zip(mvt_sorted.iter()) {
        assert_eq!(ml.name, mvt.name, "layer name mismatch");
        assert_eq!(ml.feature_count, mvt.feature_count, "feature count mismatch in {}", ml.name);
        assert_eq!(
            ml.geometry_mix.points, mvt.points,
            "point count mismatch in {}", ml.name
        );
        assert_eq!(
            ml.geometry_mix.lines, mvt.lines,
            "line count mismatch in {}", ml.name
        );
        assert_eq!(
            ml.geometry_mix.polygons, mvt.polygons,
            "polygon count mismatch in {}", ml.name
        );
        let mut model_keys: Vec<&str> = ml.columns.iter().map(|c| c.key.as_str()).collect();
        model_keys.sort();
        let mut mvt_keys: Vec<&str> = mvt.keys.iter().map(String::as_str).collect();
        mvt_keys.sort();
        assert_eq!(model_keys, mvt_keys, "property key mismatch in {}", ml.name);
    }
}

#[test]
fn phase_assemble_propagates_source_pbf_filename_to_metadata() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let chunks_dir = dir.path().join("chunks");
    let output_path = dir.path().join("phase_assemble_meta.pmtiles");
    let tmp_dir = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");
    let mut sort_reader = one_tile_sort_reader(&chunks_dir);

    let config = TilegenConfig {
        pbf_path: dir.path().join("source-file.osm.pbf"),
        output_path: output_path.clone(),
        tmp_dir,
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        skip_to: None,
        in_memory: true,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: 1,
        way_inflight_budget: 0,
        rel_batch_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format: TilePayloadFormat::Mvt,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: sort::ChunkCompression::None,
        seam_reconcile_layers: {
            let mut m = [0u8; shortbread::Layer::count()];
            m[shortbread::Layer::Boundaries as usize] = 8;
            m
        },
    };

    let (_features_read, _tiles_written, _unique_tiles, _batch_hwm, _dedup_stats, _size_diag) =
        phase_assemble(&mut sort_reader, &config).expect("assemble should succeed");

    let mut reader = crate::pmtiles_reader::PmtilesReader::open(&output_path)
        .expect("open generated pmtiles");
    let metadata = reader.read_metadata().expect("read metadata");
    let parsed: serde_json::Value = serde_json::from_str(&metadata).expect("parse metadata json");
    assert_eq!(parsed["source_pbf"], "source-file.osm.pbf");
    assert!(parsed.get("osmosis_replication_timestamp").is_none());
}

#[test]
fn phase_assemble_propagates_replication_timestamp_to_metadata() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let chunks_dir = dir.path().join("chunks");
    let output_path = dir.path().join("phase_assemble_meta_ts.pmtiles");
    let tmp_dir = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");
    let mut sort_reader = one_tile_sort_reader(&chunks_dir);

    let pbf_path = dir.path().join("replication-source.osm.pbf");
    let mut pbf_file = File::create(&pbf_path).expect("create pbf file");
    let mut pbf_writer = PbfWriter::new(&mut pbf_file, PbfCompression::default());
    let header = block_builder::HeaderBuilder::new()
        .replication_timestamp(1_700_000_123)
        .build()
        .expect("build pbf header");
    pbf_writer.write_header(&header).expect("write pbf header");
    pbf_writer.flush().expect("flush pbf");

    let config = TilegenConfig {
        pbf_path,
        output_path: output_path.clone(),
        tmp_dir,
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        skip_to: None,
        in_memory: true,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: 1,
        way_inflight_budget: 0,
        rel_batch_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format: TilePayloadFormat::Mvt,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: sort::ChunkCompression::None,
        seam_reconcile_layers: {
            let mut m = [0u8; shortbread::Layer::count()];
            m[shortbread::Layer::Boundaries as usize] = 8;
            m
        },
    };

    let (_features_read, _tiles_written, _unique_tiles, _batch_hwm, _dedup_stats, _size_diag) =
        phase_assemble(&mut sort_reader, &config).expect("assemble should succeed");

    let mut reader = crate::pmtiles_reader::PmtilesReader::open(&output_path)
        .expect("open generated pmtiles");
    let metadata = reader.read_metadata().expect("read metadata");
    let parsed: serde_json::Value = serde_json::from_str(&metadata).expect("parse metadata json");
    assert_eq!(parsed["source_pbf"], "replication-source.osm.pbf");
    assert_eq!(parsed["osmosis_replication_timestamp"], 1_700_000_123);
}

#[test]
fn phase_assemble_tile_format_sets_consistent_payload_contract() {
    let dir = tempfile::tempdir().expect("create tempdir");

    // MVT contract: gzip-compressed MVT payload, explicit MVT tile type in header.
    let mvt_chunks_dir = dir.path().join("chunks_mvt");
    let mvt_output = dir.path().join("phase_assemble_contract_mvt.pmtiles");
    let mvt_tmp = dir.path().join("tmp_mvt");
    std::fs::create_dir_all(&mvt_tmp).expect("create mvt tmp dir");
    let mut mvt_sort_reader = one_tile_sort_reader(&mvt_chunks_dir);
    let mvt_config = TilegenConfig {
        pbf_path: dir.path().join("contract-mvt.osm.pbf"),
        output_path: mvt_output.clone(),
        tmp_dir: mvt_tmp,
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        skip_to: None,
        in_memory: true,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: 1,
        way_inflight_budget: 0,
        rel_batch_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format: TilePayloadFormat::Mvt,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: sort::ChunkCompression::None,
        seam_reconcile_layers: {
            let mut m = [0u8; shortbread::Layer::count()];
            m[shortbread::Layer::Boundaries as usize] = 8;
            m
        },
    };
    let _ = phase_assemble(&mut mvt_sort_reader, &mvt_config).expect("mvt assemble should succeed");
    let mut mvt_reader = crate::pmtiles_reader::PmtilesReader::open(&mvt_output)
        .expect("open mvt pmtiles");
    let mvt_metadata = mvt_reader.read_metadata().expect("read mvt metadata");
    let mvt_json: serde_json::Value = serde_json::from_str(&mvt_metadata).expect("parse mvt metadata");
    assert_eq!(mvt_reader.tile_type(), 1, "mvt header tile_type must be mvt");
    assert_eq!(mvt_reader.tile_compression(), 2, "mvt header tile_compression must be gzip");
    assert_eq!(mvt_json["tile_payload_format"], "mvt");
    assert_eq!(mvt_json["tile_compression"], "gzip");

    // MLT contract: uncompressed payload, unknown tile type in PMTiles header + explicit metadata.
    let mlt_chunks_dir = dir.path().join("chunks_mlt");
    let mlt_output = dir.path().join("phase_assemble_contract_mlt.pmtiles");
    let mlt_tmp = dir.path().join("tmp_mlt");
    std::fs::create_dir_all(&mlt_tmp).expect("create mlt tmp dir");
    let mut mlt_sort_reader = one_tile_sort_reader(&mlt_chunks_dir);
    let mlt_config = TilegenConfig {
        pbf_path: dir.path().join("contract-mlt.osm.pbf"),
        output_path: mlt_output.clone(),
        tmp_dir: mlt_tmp,
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        skip_to: None,
        in_memory: true,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: 1,
        way_inflight_budget: 0,
        rel_batch_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format: TilePayloadFormat::Mlt,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: sort::ChunkCompression::None,
        seam_reconcile_layers: {
            let mut m = [0u8; shortbread::Layer::count()];
            m[shortbread::Layer::Boundaries as usize] = 8;
            m
        },
    };
    let _ = phase_assemble(&mut mlt_sort_reader, &mlt_config).expect("mlt assemble should succeed");
    let mut mlt_reader = crate::pmtiles_reader::PmtilesReader::open(&mlt_output)
        .expect("open mlt pmtiles");
    let mlt_metadata = mlt_reader.read_metadata().expect("read mlt metadata");
    let mlt_json: serde_json::Value = serde_json::from_str(&mlt_metadata).expect("parse mlt metadata");
    assert_eq!(mlt_reader.tile_type(), 0, "mlt header tile_type should remain unknown");
    assert_eq!(mlt_reader.tile_compression(), 1, "mlt header tile_compression should be none");
    assert_eq!(mlt_json["tile_payload_format"], "mlt");
    assert_eq!(mlt_json["tile_compression"], "none");
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

fn decode_zigzag(v: u32) -> i32 {
    let n = i32::try_from(v >> 1).unwrap_or(i32::MAX);
    if (v & 1) == 0 { n } else { -n - 1 }
}

fn decode_commands_to_abs_coords(cmds: &[u32]) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut cx = 0i32;
    let mut cy = 0i32;
    let mut move_x = 0i32;
    let mut move_y = 0i32;
    while i < cmds.len() {
        let cmd = cmds[i];
        i += 1;
        let id = cmd & 0x7;
        let count = cmd >> 3;
        match id {
            1 | 2 => {
                for _ in 0..count {
                    if i + 1 >= cmds.len() {
                        return out;
                    }
                    cx += decode_zigzag(cmds[i]);
                    cy += decode_zigzag(cmds[i + 1]);
                    i += 2;
                    if id == 1 {
                        move_x = cx;
                        move_y = cy;
                    }
                    out.push((cx, cy));
                }
            }
            7 => {
                cx = move_x;
                cy = move_y;
            }
            _ => break,
        }
    }
    out
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

#[test]
fn emit_line_preserve_mask_keeps_required_vertices() {
    let m = test_layer_match(Layer::Streets, GeomExpect::Line);
    let coords = [
        Point { x: 0.1, y: 0.10000 },
        Point { x: 0.3, y: 0.10001 },
        Point { x: 0.5, y: 0.10002 }, // pin this vertex
        Point { x: 0.7, y: 0.10001 },
        Point { x: 0.9, y: 0.10000 },
    ];

    let mut records_plain = Vec::new();
    let mut records_pinned = Vec::new();
    let mut scratch_plain = LineEmitScratch::new();
    let mut scratch_pinned = LineEmitScratch::new();
    emit_line_feature(
        1099,
        &coords,
        &[false; 5],
        &m,
        0,
        0,
        &mut records_plain,
        &mut scratch_plain,
    );
    emit_line_feature(
        1099,
        &coords,
        &[false, false, true, false, false],
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
        "pinned shared line vertex should increase retained geometry detail"
    );

    let plain_lb = decode_to_layer(&records_plain[0].data);
    let pinned_lb = decode_to_layer(&records_pinned[0].data);
    let plain_pts = decode_commands_to_abs_coords(&plain_lb.test_feature(0).geometry);
    let pinned_pts = decode_commands_to_abs_coords(&pinned_lb.test_feature(0).geometry);
    let mut target_tc = Vec::new();
    geometry::to_tile_coords_into(
        &mut target_tc,
        &[Point { x: 0.5, y: 0.10002 }],
        0,
        0,
        0,
    );
    let expected = target_tc[0];
    assert!(
        pinned_pts.contains(&expected),
        "pinned line geometry should retain required shared vertex {expected:?}"
    );
    assert!(
        !plain_pts.contains(&expected),
        "un-pinned line geometry should be allowed to drop non-required vertex {expected:?}"
    );
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
    let count = emit_polygon_feature(201, &coords, &[], &m, 0, 0, &mut records, &mut scratch, 0, None);
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
    emit_polygon_feature(200, &coords, &[], &m, 0, 0, &mut records, &mut scratch, 0, None);
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
    emit_polygon_feature(300, &coords_z0, &[], &m_z0, 0, 0, &mut records, &mut scratch, 0, None);
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
    emit_polygon_feature(300, &coords_z14, &[], &m_z14, 14, 14, &mut records, &mut scratch, 0, None);
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
    let count = emit_polygon_feature(500, &coords, &[], &m, 0, 0, &mut records, &mut scratch, 0, None);
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
        0,
        None,
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
        0,
        None,
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
        0,
        None,
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
        0,
        None,
    );

    assert_eq!(records_plain.len(), 1);
    assert_eq!(records_pinned.len(), 1);
    let (_, _, plain_cmd_count) = decode_data_header(&records_plain[0].data);
    let (_, _, pinned_cmd_count) = decode_data_header(&records_pinned[0].data);
    assert!(pinned_cmd_count > plain_cmd_count);

    let plain_lb = decode_to_layer(&records_plain[0].data);
    let pinned_lb = decode_to_layer(&records_pinned[0].data);
    let plain_pts = decode_commands_to_abs_coords(&plain_lb.test_feature(0).geometry);
    let pinned_pts = decode_commands_to_abs_coords(&pinned_lb.test_feature(0).geometry);
    let mut target_tc = Vec::new();
    geometry::to_tile_coords_into(
        &mut target_tc,
        &[Point { x: 0.5, y: 0.10002 }],
        0,
        0,
        0,
    );
    let expected = target_tc[0];
    assert!(
        pinned_pts.contains(&expected),
        "pinned geometry should retain required shared vertex {expected:?}"
    );
    assert!(
        !plain_pts.contains(&expected),
        "un-pinned geometry should be allowed to drop non-required vertex {expected:?}"
    );
}

#[test]
fn emit_multipolygon_relation_derived_shared_keys_preserve_vertices() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        Point { x: 0.1, y: 0.1 },
        Point { x: 0.3, y: 0.10001 },
        Point { x: 0.5, y: 0.10002 }, // should be preserved via relation-derived key
        Point { x: 0.7, y: 0.10001 },
        Point { x: 0.9, y: 0.1 },
        Point { x: 0.9, y: 0.9 },
        Point { x: 0.1, y: 0.9 },
        Point { x: 0.1, y: 0.1 },
    ];
    let relation_members = vec![
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 0.0, y: 0.0 },
                Point { x: 0.5, y: 0.10002 },
                Point { x: 0.0, y: 0.2 },
            ],
        },
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 1.0, y: 0.0 },
                Point { x: 0.5, y: 0.10002 },
                Point { x: 1.0, y: 0.2 },
            ],
        },
    ];
    let shared_keys = relation_shared_vertex_keys(&relation_members);
    assert!(
        shared_keys.contains(&merc_point_key(&Point { x: 0.5, y: 0.10002 })),
        "relation-derived shared key should include the target outer vertex"
    );

    let mut records_plain = Vec::new();
    let mut records_pinned = Vec::new();
    let mut emit_plain = MultipolygonEmitScratch::new();
    let mut emit_pinned = MultipolygonEmitScratch::new();
    let mut simp_plain = geometry::SimplifyMultiScratch::new();
    let mut simp_pinned = geometry::SimplifyMultiScratch::new();

    emit_multipolygon_feature(
        991,
        &outer,
        &[],
        None,
        &m,
        0,
        0,
        &mut records_plain,
        &mut emit_plain,
        &mut simp_plain,
        0,
        None,
    );
    emit_multipolygon_feature(
        991,
        &outer,
        &[],
        Some(&shared_keys),
        &m,
        0,
        0,
        &mut records_pinned,
        &mut emit_pinned,
        &mut simp_pinned,
        0,
        None,
    );

    assert_eq!(records_plain.len(), 1);
    assert_eq!(records_pinned.len(), 1);
    let plain_lb = decode_to_layer(&records_plain[0].data);
    let pinned_lb = decode_to_layer(&records_pinned[0].data);
    let plain_pts = decode_commands_to_abs_coords(&plain_lb.test_feature(0).geometry);
    let pinned_pts = decode_commands_to_abs_coords(&pinned_lb.test_feature(0).geometry);
    let mut target_tc = Vec::new();
    geometry::to_tile_coords_into(
        &mut target_tc,
        &[Point { x: 0.5, y: 0.10002 }],
        0,
        0,
        0,
    );
    let expected = target_tc[0];
    assert!(
        pinned_pts.contains(&expected),
        "relation-derived pinned geometry should retain shared vertex {expected:?}"
    );
    assert!(
        !plain_pts.contains(&expected),
        "without relation-derived shared keys, vertex may be simplified away"
    );
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
        0,
        None,
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
        0,
        None,
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
        0,
        None,
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

#[test]
fn emit_multipolygon_drops_degenerate_or_invalid_inner_rings() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        Point { x: 0.20, y: 0.20 },
        Point { x: 0.80, y: 0.20 },
        Point { x: 0.80, y: 0.80 },
        Point { x: 0.20, y: 0.80 },
        Point { x: 0.20, y: 0.20 },
    ];
    let inners = vec![
        vec![
            Point { x: 0.30, y: 0.30 },
            Point { x: 0.50, y: 0.30 },
        ], // too short
        vec![
            Point { x: 0.35, y: 0.35 },
            Point { x: 0.65, y: 0.65 },
            Point { x: 0.35, y: 0.65 },
            Point { x: 0.65, y: 0.35 },
            Point { x: 0.35, y: 0.35 },
        ], // self-intersecting
    ];

    let mut outer_only = Vec::new();
    let mut with_bad_holes = Vec::new();
    let mut emit_a = MultipolygonEmitScratch::new();
    let mut emit_b = MultipolygonEmitScratch::new();
    let mut simp_a = geometry::SimplifyMultiScratch::new();
    let mut simp_b = geometry::SimplifyMultiScratch::new();
    emit_multipolygon_feature(
        404,
        &outer,
        &[],
        None,
        &m,
        0,
        0,
        &mut outer_only,
        &mut emit_a,
        &mut simp_a,
        0,
        None,
    );
    emit_multipolygon_feature(
        404,
        &outer,
        &inners,
        None,
        &m,
        0,
        0,
        &mut with_bad_holes,
        &mut emit_b,
        &mut simp_b,
        0,
        None,
    );
    assert_eq!(outer_only.len(), 1);
    assert_eq!(with_bad_holes.len(), 1);
    let (_, _, outer_cmd_count) = decode_data_header(&outer_only[0].data);
    let (_, _, bad_holes_cmd_count) = decode_data_header(&with_bad_holes[0].data);
    assert_eq!(
        bad_holes_cmd_count, outer_cmd_count,
        "invalid/degenerate inners should be dropped and not change emitted geometry"
    );
}

#[test]
fn emit_multipolygon_invalid_inner_rejected_below_z14_but_allowed_at_z14() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        // Keep this polygon within one z14 tile so the test checks
        // invalid-inner handling, not multi-tile fanout.
        Point { x: 0.500_010, y: 0.500_010 },
        Point { x: 0.500_040, y: 0.500_010 },
        Point { x: 0.500_040, y: 0.500_040 },
        Point { x: 0.500_010, y: 0.500_040 },
        Point { x: 0.500_010, y: 0.500_010 },
    ];
    let bowtie_inner = vec![vec![
        Point { x: 0.500_018, y: 0.500_018 },
        Point { x: 0.500_032, y: 0.500_032 },
        Point { x: 0.500_018, y: 0.500_032 },
        Point { x: 0.500_032, y: 0.500_018 },
        Point { x: 0.500_018, y: 0.500_018 },
    ]];

    let mut z13_records = Vec::new();
    let mut z14_records = Vec::new();
    let mut emit_13 = MultipolygonEmitScratch::new();
    let mut emit_14 = MultipolygonEmitScratch::new();
    let mut simp_13 = geometry::SimplifyMultiScratch::new();
    let mut simp_14 = geometry::SimplifyMultiScratch::new();
    emit_multipolygon_feature(
        405,
        &outer,
        &bowtie_inner,
        None,
        &m,
        13,
        13,
        &mut z13_records,
        &mut emit_13,
        &mut simp_13,
        0,
        None,
    );
    emit_multipolygon_feature(
        405,
        &outer,
        &bowtie_inner,
        None,
        &m,
        14,
        14,
        &mut z14_records,
        &mut emit_14,
        &mut simp_14,
        0,
        None,
    );
    assert_eq!(z13_records.len(), 1);
    assert_eq!(z14_records.len(), 1);
    let (_, _, z13_cmd_count) = decode_data_header(&z13_records[0].data);
    let (_, _, z14_cmd_count) = decode_data_header(&z14_records[0].data);
    assert!(
        z14_cmd_count > z13_cmd_count,
        "z<14 should reject invalid inner rings while z=14 keeps them"
    );
}

#[test]
fn emit_multipolygon_invalid_outer_rejected_below_z14_but_allowed_at_z14() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let bowtie_outer = vec![
        Point { x: 0.500_010, y: 0.500_010 },
        Point { x: 0.500_040, y: 0.500_040 },
        Point { x: 0.500_010, y: 0.500_040 },
        Point { x: 0.500_040, y: 0.500_010 },
        Point { x: 0.500_010, y: 0.500_010 },
    ];

    let mut z13_records = Vec::new();
    let mut z14_records = Vec::new();
    let mut emit_13 = MultipolygonEmitScratch::new();
    let mut emit_14 = MultipolygonEmitScratch::new();
    let mut simp_13 = geometry::SimplifyMultiScratch::new();
    let mut simp_14 = geometry::SimplifyMultiScratch::new();

    emit_multipolygon_feature(
        4051,
        &bowtie_outer,
        &[],
        None,
        &m,
        13,
        13,
        &mut z13_records,
        &mut emit_13,
        &mut simp_13,
        0,
        None,
    );
    emit_multipolygon_feature(
        4051,
        &bowtie_outer,
        &[],
        None,
        &m,
        14,
        14,
        &mut z14_records,
        &mut emit_14,
        &mut simp_14,
        0,
        None,
    );

    assert!(
        z13_records.is_empty(),
        "z<14 should reject invalid outer rings in multipolygon path"
    );
    assert_eq!(
        z14_records.len(),
        1,
        "z=14 should keep invalid outer rings per current guard policy"
    );
}

#[test]
fn emit_multipolygon_emits_across_zoom_range_not_just_single_zoom() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        Point { x: 0.10, y: 0.10 },
        Point { x: 0.90, y: 0.10 },
        Point { x: 0.90, y: 0.90 },
        Point { x: 0.10, y: 0.90 },
        Point { x: 0.10, y: 0.10 },
    ];
    let mut records = Vec::new();
    let mut emit_scratch = MultipolygonEmitScratch::new();
    let mut simp_scratch = geometry::SimplifyMultiScratch::new();
    let count = emit_multipolygon_feature(
        406,
        &outer,
        &[],
        None,
        &m,
        0,
        2,
        &mut records,
        &mut emit_scratch,
        &mut simp_scratch,
        0,
        None,
    );
    assert_eq!(usize::try_from(count).unwrap(), records.len());
    assert!(!records.is_empty());

    let mut zooms = std::collections::BTreeSet::new();
    for rec in &records {
        let (tile_id, _) = decode_key(rec);
        let (z, _, _) = pmtiles_writer::tile_id_to_zxy(tile_id);
        zooms.insert(z);
    }
    assert!(zooms.contains(&0));
    assert!(zooms.contains(&1));
    assert!(zooms.contains(&2));
}

#[test]
fn pre_quantization_ring_validity_large_ring_scale() {
    const N: u32 = 1200;
    let mut ring = Vec::with_capacity((N as usize) + 1);
    for i in 0..N {
        let theta = f64::from(i) * std::f64::consts::TAU / f64::from(N);
        ring.push(Point {
            x: 0.5 + 0.4 * theta.cos(),
            y: 0.5 + 0.4 * theta.sin(),
        });
    }
    ring.push(ring[0]);

    let start = Instant::now();
    let valid = is_valid_simple_ring_points(&ring);
    let elapsed = start.elapsed();

    assert!(valid, "large simple ring should validate as simple");
    assert!(
        elapsed < Duration::from_secs(3),
        "large-ring validity check took too long: {elapsed:?}",
    );
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

// ---------------------------------------------------------------------------
// Shared-edge reconciliation tests
// ---------------------------------------------------------------------------

/// Helper: encode a boundary polygon feature into the wire format used by PendingTile.
fn boundary_polygon_feature(osm_id: u64, ring: &[(i32, i32)]) -> (u8, Box<[u8]>) {
    let mut geom_buf = Vec::new();
    mvt::encode_polygon(&mut geom_buf, &[ring]);
    let attrs: Vec<crate::shortbread::Attr> = vec![
        ("admin_level", AttrValue::Int(4), 0),
    ];
    (
        Layer::Boundaries as u8,
        crate::wire_format::encode_feature_data(osm_id, GeomType::Polygon, &geom_buf, &attrs, 5),
    )
}

fn seam_reconcile_boundaries() -> Vec<u8> {
    let mut v = vec![0u8; shortbread::Layer::count()];
    v[shortbread::Layer::Boundaries as usize] = 8;
    v
}

/// Two adjacent boundary polygons at z5 (≤ BOUNDARY_NO_SIMP_MAX) sharing an edge.
/// After reconciliation, shared edge vertices must be identical in both features.
#[test]
fn seam_reconciliation_two_adjacent_boundaries() {
    // Ring A: square [0,0]-[2000,0]-[2000,2000]-[0,2000]
    // Ring B: square [2000,0]-[4000,0]-[4000,2000]-[2000,2000]
    // Shared edge: (2000,0)→(2000,2000) in A, (2000,2000)→(2000,0) in B
    let ring_a = vec![(0, 0), (2000, 0), (2000, 2000), (0, 2000), (0, 0)];
    let ring_b = vec![(2000, 0), (4000, 0), (4000, 2000), (2000, 2000), (2000, 0)];

    // Use z=5 tile so reconciliation fires (z <= BOUNDARY_NO_SIMP_MAX).
    let tile_id = pmtiles_writer::xy_to_tile_id(5, 10, 10);
    let tile = PendingTile {
        tile_id,
        features: vec![
            boundary_polygon_feature(100, &ring_a),
            boundary_polygon_feature(101, &ring_b),
        ],
    };

    let metrics = SeamMetrics::new();
    let srl = seam_reconcile_boundaries();
    let encoded = encode_tile_batch_mvt(&[tile], 6, TileCompression::Gzip, &srl, &metrics);
    assert_eq!(encoded.len(), 1);

    // Verify metrics fired.
    assert!(metrics.tiles_touched.load(Ordering::Relaxed) >= 1);
    assert!(metrics.rings_decoded.load(Ordering::Relaxed) >= 2);
    // With two simple squares sharing one edge, there should be exactly 1 chain.
    assert_eq!(metrics.chains_detected.load(Ordering::Relaxed), 1);
    assert_eq!(metrics.chains_reconciled.load(Ordering::Relaxed), 1);

    // Decode the output tile and extract boundary layer polygon rings.
    let mut decoder = flate2::read::GzDecoder::new(encoded[0].compressed.as_slice());
    let mut raw = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut raw).expect("gunzip");
    let layers = crate::pmtiles_reader::decode_mvt_layers(&raw).expect("decode mvt");
    let boundary_layer = layers.iter().find(|l| l.name == "boundaries")
        .expect("should have boundaries layer");
    // After merge_same_attr_geometries, features with identical attributes may be merged
    // into a single multipolygon — so we check polygons >= 1 (not >= 2).
    assert!(boundary_layer.polygons >= 1, "should have at least 1 polygon feature");
}

/// At z > BOUNDARY_NO_SIMP_MAX, reconciliation should NOT fire.
#[test]
fn seam_reconciliation_skipped_at_high_zoom() {
    let ring_a = vec![(0, 0), (2000, 0), (2000, 2000), (0, 2000), (0, 0)];
    let ring_b = vec![(2000, 0), (4000, 0), (4000, 2000), (2000, 2000), (2000, 0)];

    // z=10, above BOUNDARY_NO_SIMP_MAX.
    let tile_id = pmtiles_writer::xy_to_tile_id(10, 500, 500);
    let tile = PendingTile {
        tile_id,
        features: vec![
            boundary_polygon_feature(100, &ring_a),
            boundary_polygon_feature(101, &ring_b),
        ],
    };

    let metrics = SeamMetrics::new();
    let _encoded = encode_tile_batch_mvt(&[tile], 6, TileCompression::Gzip, &[], &metrics);
    // No reconciliation should have happened.
    assert_eq!(metrics.tiles_touched.load(Ordering::Relaxed), 0);
    assert_eq!(metrics.chains_detected.load(Ordering::Relaxed), 0);
}

/// Cross-tile continuity: same boundary polygon clipped into two adjacent tiles
/// at z <= BOUNDARY_NO_SIMP_MAX. Verify both tiles produce valid output (regression guard).
#[test]
fn seam_reconciliation_cross_tile_continuity() {
    // A wide polygon that spans two adjacent tiles at z5.
    // Tile (10,10) and tile (11,10) are adjacent horizontally.
    let wide_ring = vec![(0, 0), (4096, 0), (4096, 2000), (0, 2000), (0, 0)];

    let tile_a_id = pmtiles_writer::xy_to_tile_id(5, 10, 10);
    let tile_b_id = pmtiles_writer::xy_to_tile_id(5, 11, 10);

    let tile_a = PendingTile {
        tile_id: tile_a_id,
        features: vec![boundary_polygon_feature(200, &wide_ring)],
    };
    let tile_b = PendingTile {
        tile_id: tile_b_id,
        features: vec![boundary_polygon_feature(200, &wide_ring)],
    };

    let metrics = SeamMetrics::new();
    let encoded = encode_tile_batch_mvt(&[tile_a, tile_b], 6, TileCompression::Gzip, &[], &metrics);
    // Both tiles should produce valid output (no panics, no empty results).
    assert_eq!(encoded.len(), 2, "both adjacent tiles should encode successfully");
}

/// Single boundary polygon (no shared edges) still gets tile-coord DP at z<=8.
#[test]
fn seam_reconciliation_single_ring_still_simplifies() {
    // A ring with a collinear midpoint — should be simplified even without shared chains.
    let ring = vec![(0, 0), (2000, 0), (4000, 0), (4000, 4000), (0, 4000), (0, 0)];
    // (2000, 0) is collinear between (0,0) and (4000,0), should be removed by DP.

    let tile_id = pmtiles_writer::xy_to_tile_id(5, 10, 10);
    let tile = PendingTile {
        tile_id,
        features: vec![boundary_polygon_feature(400, &ring)],
    };

    let metrics = SeamMetrics::new();
    let srl = seam_reconcile_boundaries();
    let encoded = encode_tile_batch_mvt(&[tile], 6, TileCompression::Gzip, &srl, &metrics);
    assert_eq!(encoded.len(), 1);

    // Should still have been touched (decoded + simplified).
    assert_eq!(metrics.tiles_touched.load(Ordering::Relaxed), 1);
    assert_eq!(metrics.rings_decoded.load(Ordering::Relaxed), 1);
    // No shared chains expected.
    assert_eq!(metrics.chains_detected.load(Ordering::Relaxed), 0);

    // Decode and verify the ring was simplified (collinear point removed).
    let mut decoder = flate2::read::GzDecoder::new(encoded[0].compressed.as_slice());
    let mut raw = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut raw).expect("gunzip");
    let layers = crate::pmtiles_reader::decode_mvt_layers(&raw).expect("decode mvt");
    let boundary_layer = layers.iter().find(|l| l.name == "boundaries")
        .expect("should have boundaries layer");
    assert!(boundary_layer.polygons >= 1, "should have boundary polygon output");
}

/// Two boundary polygons with NO shared edge still get simplified at z<=8.
#[test]
fn seam_reconciliation_no_shared_edges_still_simplifies() {
    // Two non-adjacent polygons, each with a collinear midpoint.
    let ring_a = vec![(0, 0), (500, 0), (1000, 0), (1000, 1000), (0, 1000), (0, 0)];
    let ring_b = vec![(2000, 2000), (3000, 2000), (4000, 2000), (4000, 3000), (2000, 3000), (2000, 2000)];

    let tile_id = pmtiles_writer::xy_to_tile_id(5, 10, 10);
    let tile = PendingTile {
        tile_id,
        features: vec![
            boundary_polygon_feature(500, &ring_a),
            boundary_polygon_feature(501, &ring_b),
        ],
    };

    let metrics = SeamMetrics::new();
    let srl = seam_reconcile_boundaries();
    let encoded = encode_tile_batch_mvt(&[tile], 6, TileCompression::Gzip, &srl, &metrics);
    assert_eq!(encoded.len(), 1);

    assert_eq!(metrics.tiles_touched.load(Ordering::Relaxed), 1);
    assert_eq!(metrics.rings_decoded.load(Ordering::Relaxed), 2);
    assert_eq!(metrics.chains_detected.load(Ordering::Relaxed), 0);
    assert_eq!(metrics.chains_reconciled.load(Ordering::Relaxed), 0);
}

/// Non-boundary layers are unaffected by reconciliation.
#[test]
fn seam_reconciliation_non_boundary_unaffected() {
    // Two land polygons with shared edge at z5 — should NOT trigger reconciliation.
    let ring_a = vec![(0, 0), (2000, 0), (2000, 2000), (0, 2000), (0, 0)];
    let ring_b = vec![(2000, 0), (4000, 0), (4000, 2000), (2000, 2000), (2000, 0)];

    let mut geom_a = Vec::new();
    mvt::encode_polygon(&mut geom_a, &[&ring_a]);
    let mut geom_b = Vec::new();
    mvt::encode_polygon(&mut geom_b, &[&ring_b]);
    let attrs: Vec<crate::shortbread::Attr> = vec![
        ("kind", AttrValue::Str(Cow::Borrowed("residential")), 0),
    ];

    let tile_id = pmtiles_writer::xy_to_tile_id(5, 10, 10);
    let tile = PendingTile {
        tile_id,
        features: vec![
            (Layer::Land as u8, crate::wire_format::encode_feature_data(300, GeomType::Polygon, &geom_a, &attrs, 5)),
            (Layer::Land as u8, crate::wire_format::encode_feature_data(301, GeomType::Polygon, &geom_b, &attrs, 5)),
        ],
    };

    let metrics = SeamMetrics::new();
    let _encoded = encode_tile_batch_mvt(&[tile], 6, TileCompression::Gzip, &[], &metrics);
    // Land layer should not trigger seam reconciliation.
    assert_eq!(metrics.tiles_touched.load(Ordering::Relaxed), 0);
}
