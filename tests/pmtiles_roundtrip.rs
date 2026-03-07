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

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use elivagar::pmtiles_reader::{decode_mvt_layers, PmtilesReader};
use elivagar::pmtiles_writer::{tile_id_to_zxy, xy_to_tile_id, PmtilesConfig, PmtilesWriter};
use flate2::Compression;
use flate2::write::GzEncoder;
use protohoggr::{encode_bytes_field_always, encode_varint, encode_varint_field_always};

// ---------------------------------------------------------------------------
// MVT protobuf encoder (minimal — builds tiles with named layers)
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

    // field 2: feature (one point at 0,0)
    let feature = encode_mvt_point_feature();
    encode_bytes_field_always(&mut layer, 2, &feature);

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

    // field 4: geometry (packed uint32) — MoveTo(1, dx=0, dy=0)
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
    let reader = PmtilesReader::open(path)?;
    let metadata_offset = reader.metadata_offset();
    let metadata_length = reader.metadata_length();
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
    let config = PmtilesConfig {
        min_zoom: 2,
        max_zoom: 10,
        bounds: (8.0, 54.5, 15.2, 57.8),
        center: (11.5, 56.0, 7),

    };

    let mut writer = PmtilesWriter::new(config);
    let tile = gzip_bytes(&encode_mvt_tile(&["streets"]));
    writer.add_tile(2, 0, 0, &tile).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("header_test.pmtiles");
    writer.write_to(&path).unwrap();

    let reader = PmtilesReader::open(&path).unwrap();

    // Magic + version
    assert_eq!(&reader.header()[0..7], b"PMTiles");
    assert_eq!(reader.header()[7], 3);

    // Zoom range
    assert_eq!(reader.min_zoom(), 2);
    assert_eq!(reader.max_zoom(), 10);

    // Tile type = MVT (1)
    assert_eq!(reader.tile_type(), 1);

    // Clustered = 1
    assert_eq!(reader.header()[96], 1);

    // Internal compression = gzip (2)
    assert_eq!(reader.header()[97], 2);

    // Tile compression = gzip (2)
    assert_eq!(reader.header()[98], 2);

    // Tile counts
    assert_eq!(reader.num_addressed(), 1);
    assert_eq!(reader.num_unique(), 1);

    // Bounds (E7 encoding): 8.0 -> 80000000, 54.5 -> 545000000, etc.
    let min_lon = i32::from_le_bytes(reader.header()[102..106].try_into().unwrap());
    let min_lat = i32::from_le_bytes(reader.header()[106..110].try_into().unwrap());
    let max_lon = i32::from_le_bytes(reader.header()[110..114].try_into().unwrap());
    let max_lat = i32::from_le_bytes(reader.header()[114..118].try_into().unwrap());
    assert_eq!(min_lon, 80_000_000);
    assert_eq!(min_lat, 545_000_000);
    assert_eq!(max_lon, 152_000_000);
    assert_eq!(max_lat, 578_000_000);

    // Center
    let center_zoom = reader.header()[118];
    let center_lon = i32::from_le_bytes(reader.header()[119..123].try_into().unwrap());
    let center_lat = i32::from_le_bytes(reader.header()[123..127].try_into().unwrap());
    assert_eq!(center_zoom, 7);
    assert_eq!(center_lon, 115_000_000);
    assert_eq!(center_lat, 560_000_000);
}

#[test]
fn test_tile_roundtrip() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 2,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),

    };

    let mut writer = PmtilesWriter::new(config);

    // Add tiles at z0, z1, z2 with distinct known content.
    let payloads: Vec<Vec<u8>> = (0..5)
        .map(|i| format!("tile-content-{i}").into_bytes())
        .collect();

    // Tile coords must be added in Hilbert order.
    let coords: Vec<(u8, u32, u32)> = vec![
        (0, 0, 0),
        (1, 0, 0),
        (1, 0, 1),
        (1, 1, 1),
        (1, 1, 0),
    ];

    let gzipped: Vec<Vec<u8>> = payloads.iter().map(|p| gzip_bytes(p)).collect();
    for (i, &(z, x, y)) in coords.iter().enumerate() {
        writer.add_tile(z, x, y, &gzipped[i]).unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roundtrip.pmtiles");
    writer.write_to(&path).unwrap();

    // Read back and verify every tile matches its original payload.
    let mut reader = PmtilesReader::open(&path).unwrap();
    let entries = reader.read_all_entries().unwrap();
    assert_eq!(entries.len(), 5);

    for (i, entry) in entries.iter().enumerate() {
        let (z, x, y) = tile_id_to_zxy(entry.tile_id);
        assert_eq!((z, x, y), coords[i], "tile {i} coord mismatch");

        let decompressed = reader.read_tile(entry).unwrap();
        assert_eq!(decompressed, payloads[i], "tile {i} data mismatch at z{z}/{x}/{y}");
    }
}

#[test]
fn test_mvt_layer_decode() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 5,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),

    };

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
    let mut reader = PmtilesReader::open(&path).unwrap();
    let entries = reader.read_all_entries().unwrap();
    assert_eq!(entries.len(), 3);

    for (i, entry) in entries.iter().enumerate() {
        let raw = reader.read_tile(entry).unwrap();
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

    // Also verify metadata contains Shortbread layer definitions.
    let mut reader = PmtilesReader::open(&path).unwrap();
    let metadata = reader.read_metadata().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(parsed["name"], "Shortbread");
    let vector_layers = parsed["vector_layers"].as_array().unwrap();
    assert_eq!(vector_layers.len(), 26);
}

#[test]
fn test_deduplication() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 2,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),

    };

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
    let mut reader = PmtilesReader::open(&path).unwrap();
    assert_eq!(reader.num_addressed(), 5);
    assert_eq!(reader.num_unique(), 2);

    // All 5 tiles should still be readable and decompressible.
    let entries = reader.read_all_entries().unwrap();
    assert_eq!(entries.len(), 5);

    for entry in &entries {
        let raw = reader.read_tile(entry).unwrap();
        let layers = decode_mvt_layers(&raw).unwrap();
        assert!(!layers.is_empty(), "tile should have at least one layer");
    }

    // Verify the unique tile has the right layers.
    // z1/1/1 is tile_id = xy_to_tile_id(1,1,1) = 3
    let unique_id = xy_to_tile_id(1, 1, 1);
    let unique_entry = entries.iter().find(|e| e.tile_id == unique_id).unwrap();
    let layers = decode_mvt_layers(&reader.read_tile(unique_entry).unwrap()).unwrap();
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0].name, "streets");
    assert_eq!(layers[1].name, "buildings");
}

/// Full pipeline integration test — requires a PBF file.
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
        skip_to: None,
        in_memory: false,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: std::thread::available_parallelism().map(std::num::NonZero::get).unwrap_or(4),
        way_inflight_budget: 0,
        rel_batch_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format: elivagar::TilePayloadFormat::Mvt,
        tile_compression: elivagar::TileCompression::Gzip,
        compress_sort_chunks: elivagar::sort::ChunkCompression::None,
        seam_reconcile_layers: {
            let mut m = [0u8; elivagar::shortbread::Layer::count()];
            m[elivagar::shortbread::Layer::Boundaries as usize] = 8;
            m
        },
        fanout_caps: [0; elivagar::shortbread::Layer::count()],
        polygon_simplify_factor: 1.0,
    };

    elivagar::run(&config).expect("pipeline should succeed");

    // Open and validate the output PMTiles.
    let mut reader = PmtilesReader::open(&output).unwrap();
    assert_eq!(reader.min_zoom(), 0);
    assert_eq!(reader.max_zoom(), 14);
    assert_eq!(reader.tile_type(), 1); // MVT

    let entries = reader.read_all_entries().unwrap();
    assert!(
        entries.len() > 100,
        "expected many tiles, got {}",
        entries.len()
    );

    // Verify tiles exist at multiple zoom levels.
    let zooms: HashSet<u8> = entries.iter().map(|e| tile_id_to_zxy(e.tile_id).0).collect();
    assert!(
        zooms.len() >= 5,
        "expected tiles at 5+ zoom levels, got {}: {:?}",
        zooms.len(),
        zooms
    );

    // Valid Shortbread layer names (all 26).
    let shortbread_layers: HashSet<&str> = [
        "water_polygons", "water_polygons_labels",
        "water_lines", "water_lines_labels",
        "dam_lines", "dam_polygons",
        "pier_lines", "pier_polygons",
        "boundaries", "boundary_labels",
        "place_labels", "land", "sites",
        "buildings", "addresses",
        "streets", "street_polygons",
        "street_labels", "street_labels_points",
        "streets_polygons_labels",
        "bridges", "aerialways", "ferries",
        "public_transport", "pois", "ocean",
    ].iter().copied().collect();

    // Sample tiles across zoom levels and verify MVT content.
    let sample_count = entries.len().min(200);
    let step = entries.len() / sample_count;
    let mut total_layers_seen: HashSet<String> = HashSet::new();

    for entry in entries.iter().step_by(step.max(1)).take(sample_count) {
        let raw = reader.read_tile(entry).unwrap();
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
    let mut reader = PmtilesReader::open(&output).unwrap();
    let metadata = reader.read_metadata().unwrap();
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
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 2,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

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
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 0,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

    let mut writer = PmtilesWriter::new(config);

    // Write a valid gzip header but truncated content — will fail decompression.
    let broken_gzip = vec![0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xff];
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
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 0,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

    let mut writer = PmtilesWriter::new(config);
    let mvt = encode_mvt_tile(&["streets"]);
    writer.add_tile(0, 0, 0, &gzip_bytes(&mvt)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verify_bad_metadata_json.pmtiles");
    writer.write_to(&path).unwrap();

    rewrite_metadata_json_in_place(&path, "{");

    let err = elivagar::verify::verify(&path).err().expect("verify should fail");
    let msg = err.to_string();
    assert!(msg.contains("metadata error"), "unexpected error: {msg}");
    assert!(msg.contains("invalid JSON"), "unexpected error: {msg}");
}

#[test]
fn test_verify_fail_metadata_missing_vector_layers() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 0,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

    let mut writer = PmtilesWriter::new(config);
    let mvt = encode_mvt_tile(&["streets"]);
    writer.add_tile(0, 0, 0, &gzip_bytes(&mvt)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verify_missing_vector_layers.pmtiles");
    writer.write_to(&path).unwrap();

    rewrite_metadata_json_in_place(&path, "{\"name\":\"Shortbread\",\"format\":\"pbf\"}");

    let err = elivagar::verify::verify(&path).err().expect("verify should fail");
    let msg = err.to_string();
    assert!(msg.contains("metadata error"), "unexpected error: {msg}");
    assert!(msg.contains("missing 'vector_layers'"), "unexpected error: {msg}");
}

#[test]
fn test_verify_fail_metadata_vector_layers_schema() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 0,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

    let mut writer = PmtilesWriter::new(config);
    let mvt = encode_mvt_tile(&["streets"]);
    writer.add_tile(0, 0, 0, &gzip_bytes(&mvt)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verify_bad_vector_layers_schema.pmtiles");
    writer.write_to(&path).unwrap();

    // vector_layers exists, but entry is missing string "id".
    rewrite_metadata_json_in_place(&path, "{\"vector_layers\":[{\"id\":1}]}");

    let err = elivagar::verify::verify(&path).err().expect("verify should fail");
    let msg = err.to_string();
    assert!(msg.contains("metadata error"), "unexpected error: {msg}");
    assert!(
        msg.contains("vector_layers entry missing 'id' string"),
        "unexpected error: {msg}"
    );
}

#[test]
fn test_metadata_rewrite_rejects_oversized_replacement() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 0,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

    let mut writer = PmtilesWriter::new(config);
    let mvt = encode_mvt_tile(&["streets"]);
    writer.add_tile(0, 0, 0, &gzip_bytes(&mvt)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verify_oversize_metadata_rewrite.pmtiles");
    writer.write_to(&path).unwrap();

    let reader = PmtilesReader::open(&path).unwrap();
    let metadata_length = reader.metadata_length() as usize;

    // Build increasingly larger low-compressibility JSON until the gzipped
    // replacement exceeds the archive's metadata section.
    let mut item_count = 512usize;
    let oversized_json = loop {
        let mut items = String::new();
        for i in 0..item_count {
            let ch = char::from_u32(33 + ((i % 90) as u32)).unwrap();
            items.push_str(&format!(r#""{:08x}-{}-{:08x}","#, i, ch, i.wrapping_mul(1_048_583)));
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
        err.to_string().contains("replacement metadata must fit existing section"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_verify_fail_suspicious_geometry_delta() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 0,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

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
