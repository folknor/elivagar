#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

//! Integration tests for PMTiles output validity.
//!
//! Tests that tiles written by `PmtilesWriter` can be read back and decoded
//! correctly. The always-run tests use synthetic tiles; the `#[ignore]` test
//! runs the full pipeline on a real PBF.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use elivagar::archive::{
    Archive, BlobRef, Clustering, Compression as HeaderCompression, E7Bbox, E7Center, LonLatBbox,
    MapCenter, PmtilesConfig, PmtilesWriter, TileRuns, TileType,
};
use elivagar::tile_detail::{Strictness, decode_detail_tile};
use elivagar::tiling::TileId;

fn xy_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
    TileId::at(z, x, y).raw()
}

fn tile_id_to_zxy(tile_id: u64) -> (u8, u32, u32) {
    TileId::new(tile_id).expect("addressable tile id").coord()
}
use flate2::Compression;
use flate2::write::GzEncoder;
use protohoggr::{encode_bytes_field_always, encode_varint, encode_varint_field_always};

/// One addressed tile and the payload it names.
struct Entry {
    tile_id: u64,
    blob: BlobRef,
}

fn expand_runs(runs: &TileRuns) -> Vec<Entry> {
    let mut entries = Vec::new();
    for run in runs.as_slice() {
        for tile_id in run.start().raw()..run.end() {
            entries.push(Entry {
                tile_id,
                blob: run.blob(),
            });
        }
    }
    entries
}

struct LayerSummary {
    name: String,
    feature_count: usize,
}

fn decode_mvt_layers(raw: &[u8]) -> Result<Vec<LayerSummary>, String> {
    Ok(decode_detail_tile(raw, Strictness::Tolerant)?
        .layers
        .iter()
        .map(|layer| LayerSummary {
            name: layer.name.to_string(),
            feature_count: layer.features.len(),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// MVT protobuf encoder (minimal - builds tiles with named layers)
// ---------------------------------------------------------------------------

/// Encode a minimal valid MVT tile with the given layers.
/// Each layer gets one point feature at (0,0).
fn encode_mvt_tile(layer_names: &[&str]) -> Vec<u8> {
    let mut tile = Vec::new();
    for name in layer_names {
        let layer_bytes = encode_mvt_layer(name);
        encode_bytes_field_always(&mut tile, 3, &layer_bytes);
    }
    tile
}

fn encode_mvt_layer(name: &str) -> Vec<u8> {
    let mut layer = Vec::new();

    // field 1: name (string)
    encode_bytes_field_always(&mut layer, 1, name.as_bytes());

    // field 2: feature (one point at 0,0). Targeted paint-order layers need
    // their required attributes even in this minimal valid fixture.
    let feature = if name == "streets" {
        encode_mvt_point_feature_with_kind()
    } else {
        encode_mvt_point_feature()
    };
    encode_bytes_field_always(&mut layer, 2, &feature);

    if name == "streets" {
        encode_bytes_field_always(&mut layer, 3, b"kind");
        let mut value = Vec::new();
        encode_bytes_field_always(&mut value, 1, b"residential");
        encode_bytes_field_always(&mut layer, 4, &value);
    }

    // field 5: extent (varint) = 4096
    encode_varint_field_always(&mut layer, 5, 4096);

    // field 15: version (varint) = 2
    encode_varint_field_always(&mut layer, 15, 2);

    layer
}

fn encode_mvt_layer_with_feature(name: &str, feature: &[u8]) -> Vec<u8> {
    let mut layer = Vec::new();
    encode_bytes_field_always(&mut layer, 1, name.as_bytes());
    encode_bytes_field_always(&mut layer, 2, feature);
    encode_varint_field_always(&mut layer, 5, 4096);
    encode_varint_field_always(&mut layer, 15, 2);
    layer
}

fn encode_mvt_tile_with_layers(layers: &[Vec<u8>]) -> Vec<u8> {
    let mut tile = Vec::new();
    for layer in layers {
        encode_bytes_field_always(&mut tile, 3, layer);
    }
    tile
}

fn encode_mvt_point_feature() -> Vec<u8> {
    let mut feature = Vec::new();

    // field 3: type = POINT (1)
    encode_varint_field_always(&mut feature, 3, 1);

    // field 4: geometry (packed uint32) - MoveTo(1, dx=0, dy=0)
    // MoveTo command: (1 << 3) | 1 = 9
    // param 0 (zigzag): 0
    // param 1 (zigzag): 0
    let mut geom = Vec::new();
    encode_varint(&mut geom, 9); // MoveTo, count=1
    encode_varint(&mut geom, 0); // dx=0
    encode_varint(&mut geom, 0); // dy=0
    encode_bytes_field_always(&mut feature, 4, &geom);

    feature
}

fn encode_mvt_point_feature_with_kind() -> Vec<u8> {
    let mut feature = Vec::new();
    // field 2: one (key=0, value=0) tag pair.
    encode_bytes_field_always(&mut feature, 2, &[0, 0]);
    encode_varint_field_always(&mut feature, 3, 1);
    let mut geom = Vec::new();
    encode_varint(&mut geom, 9);
    encode_varint(&mut geom, 0);
    encode_varint(&mut geom, 0);
    encode_bytes_field_always(&mut feature, 4, &geom);
    feature
}

fn zigzag_encode_i64(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

fn encode_mvt_linestring_feature_with_delta(dx: i64, dy: i64) -> Vec<u8> {
    let mut feature = Vec::new();
    encode_varint_field_always(&mut feature, 3, 2); // LineString
    let mut geom = Vec::new();
    encode_varint(&mut geom, 9); // MoveTo, count=1
    encode_varint(&mut geom, 0);
    encode_varint(&mut geom, 0);
    encode_varint(&mut geom, 10); // LineTo, count=1
    encode_varint(&mut geom, zigzag_encode_i64(dx));
    encode_varint(&mut geom, zigzag_encode_i64(dy));
    encode_bytes_field_always(&mut feature, 4, &geom);
    feature
}

// ---------------------------------------------------------------------------
// Gzip helper
// ---------------------------------------------------------------------------

fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn try_rewrite_metadata_json_in_place(path: &Path, json: &str) -> io::Result<()> {
    let reader = Archive::open(path)?;
    let metadata_offset = reader.header().metadata.offset;
    let metadata_length = reader.header().metadata.length;
    let replacement = gzip_bytes(json.as_bytes());
    if replacement.len() > metadata_length as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "replacement metadata must fit existing section ({} > {})",
                replacement.len(),
                metadata_length
            ),
        ));
    }

    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(metadata_offset))?;
    file.write_all(&replacement)?;
    let pad = metadata_length as usize - replacement.len();
    if pad > 0 {
        file.write_all(&vec![0u8; pad])?;
    }
    Ok(())
}

fn rewrite_metadata_json_in_place(path: &Path, json: &str) {
    try_rewrite_metadata_json_in_place(path, json).unwrap();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_header_fields() {
    let config = PmtilesConfig::new(
        2,
        10,
        LonLatBbox {
            west: 8.0,
            south: 54.5,
            east: 15.2,
            north: 57.8,
        },
        MapCenter {
            lon: 11.5,
            lat: 56.0,
            zoom: 7,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);
    let tile = gzip_bytes(&encode_mvt_tile(&["streets"]));
    writer.add_tile(2, 0, 0, &tile).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("header_test.pmtiles");
    writer.write_to(&path).unwrap();

    // Magic and version are checked by the decode itself.
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[0..7], b"PMTiles");
    assert_eq!(bytes[7], 3);

    let reader = Archive::open(&path).unwrap();
    let header = reader.header();
    assert_eq!((header.min_zoom, header.max_zoom), (2, 10));
    assert_eq!(header.tile_type, TileType::Mvt);
    assert_eq!(header.clustering, Clustering::Clustered);
    assert_eq!(header.internal_compression, HeaderCompression::Gzip);
    assert_eq!(header.tile_compression, HeaderCompression::Gzip);
    assert_eq!((header.addressed_tiles, header.tile_contents), (1, 1));
    // E7 encoding: 8.0 -> 80000000, 54.5 -> 545000000, etc.
    assert_eq!(
        header.bounds,
        E7Bbox {
            west: 80_000_000,
            south: 545_000_000,
            east: 152_000_000,
            north: 578_000_000,
        }
    );
    assert_eq!(
        header.center,
        E7Center {
            lon: 115_000_000,
            lat: 560_000_000,
            zoom: 7,
        }
    );
}

#[test]
fn test_tile_roundtrip() {
    let config = PmtilesConfig::new(
        0,
        2,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 0,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);

    // Add tiles at z0, z1, z2 with distinct known content.
    let payloads: Vec<Vec<u8>> = (0..5)
        .map(|i| format!("tile-content-{i}").into_bytes())
        .collect();

    // Tile coords must be added in Hilbert order.
    let coords: Vec<(u8, u32, u32)> = vec![(0, 0, 0), (1, 0, 0), (1, 0, 1), (1, 1, 1), (1, 1, 0)];

    let gzipped: Vec<Vec<u8>> = payloads.iter().map(|p| gzip_bytes(p)).collect();
    for (i, &(z, x, y)) in coords.iter().enumerate() {
        writer.add_tile(z, x, y, &gzipped[i]).unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roundtrip.pmtiles");
    writer.write_to(&path).unwrap();

    // Read back and verify every tile matches its original payload.
    let reader = Archive::open(&path).unwrap();
    let entries = expand_runs(&reader.read_all_runs().unwrap());
    assert_eq!(entries.len(), 5);

    for (i, entry) in entries.iter().enumerate() {
        let (z, x, y) = tile_id_to_zxy(entry.tile_id);
        assert_eq!((z, x, y), coords[i], "tile {i} coord mismatch");

        let decompressed = reader.read_tile(entry.blob).unwrap();
        assert_eq!(
            decompressed, payloads[i],
            "tile {i} data mismatch at z{z}/{x}/{y}"
        );
    }
}

#[test]
fn test_mvt_layer_decode() {
    let config = PmtilesConfig::new(
        0,
        5,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 0,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);

    // Write tiles with known MVT layer content.
    let layer_names_per_tile: Vec<Vec<&str>> = vec![
        vec!["streets", "buildings"],
        vec!["water_polygons"],
        vec!["streets", "boundaries", "land"],
    ];

    // z0, z1/0/0, z1/0/1 in Hilbert order.
    let coords = [(0u8, 0u32, 0u32), (1, 0, 0), (1, 0, 1)];

    for (i, &(z, x, y)) in coords.iter().enumerate() {
        let mvt = encode_mvt_tile(&layer_names_per_tile[i]);
        let gzipped = gzip_bytes(&mvt);
        writer.add_tile(z, x, y, &gzipped).unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mvt_decode.pmtiles");
    writer.write_to(&path).unwrap();

    // Read back and decode MVT layers.
    let reader = Archive::open(&path).unwrap();
    let entries = expand_runs(&reader.read_all_runs().unwrap());
    assert_eq!(entries.len(), 3);

    for (i, entry) in entries.iter().enumerate() {
        let raw = reader.read_tile(entry.blob).unwrap();
        let layers = decode_mvt_layers(&raw).unwrap();

        let expected = &layer_names_per_tile[i];
        assert_eq!(
            layers.len(),
            expected.len(),
            "tile {i}: expected {} layers, got {}",
            expected.len(),
            layers.len()
        );

        for (j, layer) in layers.iter().enumerate() {
            assert_eq!(
                layer.name, expected[j],
                "tile {i} layer {j}: expected '{}', got '{}'",
                expected[j], layer.name
            );
            assert_eq!(
                layer.feature_count, 1,
                "tile {i} layer '{}': expected 1 feature, got {}",
                layer.name, layer.feature_count
            );
        }
    }

    // Also verify metadata contains Shortbread layer definitions. A z0-z5
    // archive advertises only the layers whose zoom range reaches z5.
    let reader = Archive::open(&path).unwrap();
    let metadata = reader.metadata().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(parsed["name"], "Shortbread");
    let vector_layers = parsed["vector_layers"].as_array().unwrap();
    let reachable = elivagar::shortbread::Layer::ALL
        .iter()
        .filter(|layer| layer.min_zoom() <= 5)
        .count();
    assert!(reachable > 0 && reachable < 26);
    assert_eq!(vector_layers.len(), reachable);
    for layer in vector_layers {
        assert!(layer["minzoom"].as_u64().unwrap() <= 5);
        assert_eq!(layer["maxzoom"], 5);
    }
}

#[test]
fn test_deduplication() {
    let config = PmtilesConfig::new(
        0,
        2,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 0,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);

    // Same content for multiple tiles.
    let shared = gzip_bytes(&encode_mvt_tile(&["water_polygons"]));
    let unique = gzip_bytes(&encode_mvt_tile(&["streets", "buildings"]));

    // z0 and z1 tiles: 5 total, 4 share the same content.
    writer.add_tile(0, 0, 0, &shared).unwrap();
    writer.add_tile(1, 0, 0, &shared).unwrap();
    writer.add_tile(1, 0, 1, &shared).unwrap();
    writer.add_tile(1, 1, 1, &unique).unwrap();
    writer.add_tile(1, 1, 0, &shared).unwrap();

    assert_eq!(writer.tile_count(), 5);
    assert_eq!(writer.unique_tile_count(), 2);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dedup.pmtiles");
    writer.write_to(&path).unwrap();

    // Read back header counts.
    let reader = Archive::open(&path).unwrap();
    assert_eq!(reader.header().addressed_tiles, 5);
    assert_eq!(reader.header().tile_contents, 2);

    // All 5 tiles should still be readable and decompressible.
    let entries = expand_runs(&reader.read_all_runs().unwrap());
    assert_eq!(entries.len(), 5);

    for entry in &entries {
        let raw = reader.read_tile(entry.blob).unwrap();
        let layers = decode_mvt_layers(&raw).unwrap();
        assert!(!layers.is_empty(), "tile should have at least one layer");
    }

    // Verify the unique tile has the right layers.
    // z1/1/1 is tile_id = xy_to_tile_id(1,1,1) = 3
    let unique_id = xy_to_tile_id(1, 1, 1);
    let unique_entry = entries.iter().find(|e| e.tile_id == unique_id).unwrap();
    let layers = decode_mvt_layers(&reader.read_tile(unique_entry.blob).unwrap()).unwrap();
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0].name, "streets");
    assert_eq!(layers[1].name, "buildings");
}

/// Full pipeline integration test - requires a PBF file.
///
/// Set `ELIVAGAR_TEST_PBF` to override the default path.
/// Run with: cargo test --test pmtiles_roundtrip -- --ignored
#[test]
#[ignore]
#[allow(clippy::too_many_lines)]
fn test_full_pipeline() {
    let pbf_path = std::env::var("ELIVAGAR_TEST_PBF")
        .unwrap_or_else(|_| "data/denmark-latest.osm.pbf".to_string());

    if !Path::new(&pbf_path).exists() {
        eprintln!("Skipping: PBF not found at {pbf_path}");
        eprintln!("Set ELIVAGAR_TEST_PBF to a valid PBF path.");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("pipeline_test.pmtiles");
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();

    let config = elivagar::TilegenConfig {
        pbf_path: pbf_path.into(),
        output_path: output.clone(),
        tmp_dir: tmp,
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        ocean_tiles: None,
        skip_to: None,
        in_memory: false,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: elivagar::resources::default_threads(),
        way_inflight_budget: None,
        assemble_batch_budget: None,
        dedup_cap: None,
        sort_chunk_size: None,
        locations_on_ways: false,
        tile_format: elivagar::TilePayloadFormat::Mvt,
        tile_compression: elivagar::TileCompression::Gzip,
        compress_sort_chunks: elivagar::sort::ChunkCompression::None,
        fanout_caps: [None; elivagar::shortbread::Layer::count()],
        polygon_simplify_factor: 1.0,
    };

    elivagar::run(&config).expect("pipeline should succeed");

    // Open and validate the output PMTiles.
    let reader = Archive::open(&output).unwrap();
    assert_eq!(reader.header().min_zoom, 0);
    assert_eq!(reader.header().max_zoom, 14);
    assert_eq!(reader.header().tile_type, TileType::Mvt);

    let entries = expand_runs(&reader.read_all_runs().unwrap());
    assert!(
        entries.len() > 100,
        "expected many tiles, got {}",
        entries.len()
    );

    // Verify tiles exist at multiple zoom levels.
    let zooms: BTreeSet<u8> = entries
        .iter()
        .map(|e| tile_id_to_zxy(e.tile_id).0)
        .collect();
    assert!(
        zooms.len() >= 5,
        "expected tiles at 5+ zoom levels, got {}: {:?}",
        zooms.len(),
        zooms
    );

    // Valid Shortbread layer names (all 26).
    let shortbread_layers: BTreeSet<&str> = [
        "water_polygons",
        "water_polygons_labels",
        "water_lines",
        "water_lines_labels",
        "dam_lines",
        "dam_polygons",
        "pier_lines",
        "pier_polygons",
        "boundaries",
        "boundary_labels",
        "place_labels",
        "land",
        "sites",
        "buildings",
        "addresses",
        "streets",
        "street_polygons",
        "street_labels",
        "street_labels_points",
        "streets_polygons_labels",
        "bridges",
        "aerialways",
        "ferries",
        "public_transport",
        "pois",
        "ocean",
    ]
    .iter()
    .copied()
    .collect();

    // Sample tiles across zoom levels and verify MVT content.
    let sample_count = entries.len().min(200);
    let step = entries.len() / sample_count;
    let mut total_layers_seen: BTreeSet<String> = BTreeSet::new();

    for entry in entries.iter().step_by(step.max(1)).take(sample_count) {
        let raw = reader.read_tile(entry.blob).unwrap();
        let layers = decode_mvt_layers(&raw).unwrap();

        assert!(
            !layers.is_empty(),
            "tile {} should have at least one layer",
            entry.tile_id
        );

        for layer in &layers {
            assert!(
                shortbread_layers.contains(layer.name.as_str()),
                "unexpected layer name '{}' in tile {}",
                layer.name,
                entry.tile_id
            );
            assert!(
                layer.feature_count > 0,
                "layer '{}' in tile {} has no features",
                layer.name,
                entry.tile_id
            );
            total_layers_seen.insert(layer.name.clone());
        }
    }

    // We should see a reasonable variety of layers across all sampled tiles.
    assert!(
        total_layers_seen.len() >= 5,
        "expected 5+ distinct layers across sampled tiles, got {}: {:?}",
        total_layers_seen.len(),
        total_layers_seen
    );

    // Verify metadata.
    let reader = Archive::open(&output).unwrap();
    let metadata = reader.metadata().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(parsed["name"], "Shortbread");
    assert_eq!(parsed["format"], "pbf");
    let vector_layers = parsed["vector_layers"].as_array().unwrap();
    assert_eq!(vector_layers.len(), 26);
}

// ---------------------------------------------------------------------------
// Verify integration tests
// ---------------------------------------------------------------------------

#[test]
fn test_verify_pass_synthetic() {
    let config = PmtilesConfig::new(
        0,
        2,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 0,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);

    let coords = [(0u8, 0u32, 0u32), (1, 0, 0), (1, 0, 1)];
    for &(z, x, y) in &coords {
        let mvt = encode_mvt_tile(&["streets", "water_polygons"]);
        let gzipped = gzip_bytes(&mvt);
        writer.add_tile(z, x, y, &gzipped).unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verify_pass.pmtiles");
    writer.write_to(&path).unwrap();

    let report = elivagar::verify::verify(&path).unwrap();
    assert!(report.passed, "expected verify to pass");
    assert_eq!(report.tiles_checked, 3);
    assert!(report.tile_errors.is_empty());
    assert!(report.layers_observed.contains("streets"));
    assert!(report.layers_observed.contains("water_polygons"));
}

#[test]
fn test_verify_fail_truncated_tile() {
    let config = PmtilesConfig::new(
        0,
        0,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 0,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);

    // Write a valid gzip header but truncated content - will fail decompression.
    let broken_gzip = vec![
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xff,
    ];
    writer.add_tile(0, 0, 0, &broken_gzip).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verify_fail.pmtiles");
    writer.write_to(&path).unwrap();

    let report = elivagar::verify::verify(&path).unwrap();
    assert!(!report.passed, "expected verify to fail");
    assert!(!report.tile_errors.is_empty());
}

#[test]
fn test_verify_fail_invalid_metadata_json() {
    let config = PmtilesConfig::new(
        0,
        0,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 0,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);
    let mvt = encode_mvt_tile(&["streets"]);
    writer.add_tile(0, 0, 0, &gzip_bytes(&mvt)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verify_bad_metadata_json.pmtiles");
    writer.write_to(&path).unwrap();

    rewrite_metadata_json_in_place(&path, "{");

    let err = elivagar::verify::verify(&path)
        .err()
        .expect("verify should fail");
    let msg = err.to_string();
    assert!(msg.contains("metadata error"), "unexpected error: {msg}");
    assert!(msg.contains("invalid JSON"), "unexpected error: {msg}");
}

#[test]
fn test_verify_fail_metadata_missing_vector_layers() {
    let config = PmtilesConfig::new(
        0,
        0,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 0,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);
    let mvt = encode_mvt_tile(&["streets"]);
    writer.add_tile(0, 0, 0, &gzip_bytes(&mvt)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verify_missing_vector_layers.pmtiles");
    writer.write_to(&path).unwrap();

    rewrite_metadata_json_in_place(&path, "{\"name\":\"Shortbread\",\"format\":\"pbf\"}");

    let err = elivagar::verify::verify(&path)
        .err()
        .expect("verify should fail");
    let msg = err.to_string();
    assert!(msg.contains("metadata error"), "unexpected error: {msg}");
    assert!(
        msg.contains("missing 'vector_layers'"),
        "unexpected error: {msg}"
    );
}

#[test]
fn test_verify_fail_metadata_vector_layers_schema() {
    let config = PmtilesConfig::new(
        0,
        0,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 0,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);
    let mvt = encode_mvt_tile(&["streets"]);
    writer.add_tile(0, 0, 0, &gzip_bytes(&mvt)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verify_bad_vector_layers_schema.pmtiles");
    writer.write_to(&path).unwrap();

    // vector_layers exists, but entry is missing string "id".
    rewrite_metadata_json_in_place(&path, "{\"vector_layers\":[{\"id\":1}]}");

    let err = elivagar::verify::verify(&path)
        .err()
        .expect("verify should fail");
    let msg = err.to_string();
    assert!(msg.contains("metadata error"), "unexpected error: {msg}");
    assert!(
        msg.contains("vector_layers entry missing 'id' string"),
        "unexpected error: {msg}"
    );
}

#[test]
fn test_metadata_rewrite_rejects_oversized_replacement() {
    let config = PmtilesConfig::new(
        0,
        0,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 0,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);
    let mvt = encode_mvt_tile(&["streets"]);
    writer.add_tile(0, 0, 0, &gzip_bytes(&mvt)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verify_oversize_metadata_rewrite.pmtiles");
    writer.write_to(&path).unwrap();

    let reader = Archive::open(&path).unwrap();
    let metadata_length = reader.header().metadata.length as usize;

    // Build increasingly larger low-compressibility JSON until the gzipped
    // replacement exceeds the archive's metadata section.
    let mut item_count = 512usize;
    let oversized_json = loop {
        let mut items = String::new();
        for i in 0..item_count {
            let ch = char::from_u32(33 + ((i % 90) as u32)).unwrap();
            items.push_str(&format!(
                r#""{:08x}-{}-{:08x}","#,
                i,
                ch,
                i.wrapping_mul(1_048_583)
            ));
        }
        let candidate = format!(r#"{{"vector_layers":[{{"id":"streets"}}],"pad":[{items}]}}"#);
        if gzip_bytes(candidate.as_bytes()).len() > metadata_length {
            break candidate;
        }
        item_count *= 2;
    };

    let err = try_rewrite_metadata_json_in_place(&path, &oversized_json)
        .expect_err("oversize replacement should fail");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(
        err.to_string()
            .contains("replacement metadata must fit existing section"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_verify_fail_suspicious_geometry_delta() {
    let config = PmtilesConfig::new(
        0,
        0,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 0,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);
    let feature = encode_mvt_linestring_feature_with_delta(500_000, 0);
    let layer = encode_mvt_layer_with_feature("streets", &feature);
    let tile = encode_mvt_tile_with_layers(&[layer]);
    writer.add_tile(0, 0, 0, &gzip_bytes(&tile)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verify_bad_geom_delta.pmtiles");
    writer.write_to(&path).unwrap();

    let report = elivagar::verify::verify(&path).unwrap();
    assert!(!report.passed, "expected verify to fail");
    assert!(
        report
            .tile_errors
            .iter()
            .any(|e| e.contains("suspicious geometry delta")),
        "expected geometry delta error, got {:?}",
        report.tile_errors
    );
}
