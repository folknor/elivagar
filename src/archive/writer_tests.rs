use super::*;
use protohoggr::encode_varint;

fn xy_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
    TileId::at(z, x, y).raw()
}

fn tid(raw: u64) -> TileId {
    TileId::new(raw).unwrap()
}

fn tile_id_to_zxy(tile_id: u64) -> (u8, u32, u32) {
    TileId::new(tile_id).unwrap().coord()
}

/// The header of an archive written to bytes.
fn header_of(bytes: &[u8]) -> Header {
    Header::decode(bytes[..HEADER_LEN].try_into().unwrap()).unwrap()
}

#[test]
fn header_coordinates_round_to_nearest_e7() {
    let config = PmtilesConfig::new(
        0,
        14,
        LonLatBbox {
            west: -12.34567896,
            south: -55.12345676,
            east: 12.34567896,
            north: 55.12345676,
        },
        MapCenter {
            lon: -0.00000006,
            lat: 0.00000006,
            zoom: 2,
        },
    )
    .expect("valid archive config");
    let mut writer = PmtilesWriter::new(config);
    writer.add_tile(0, 0, 0, &[1]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e7.pmtiles");
    writer.write_to(&path).unwrap();
    let header = header_of(&std::fs::read(&path).unwrap());
    assert_eq!(
        header.bounds,
        E7Bbox {
            west: -123_456_790,
            south: -551_234_568,
            east: 123_456_790,
            north: 551_234_568,
        }
    );
    assert_eq!(
        header.center,
        E7Center {
            lon: -1,
            lat: 1,
            zoom: 2
        }
    );
}

#[test]
fn config_refuses_invalid_geography_before_any_tile_work() {
    let world = LonLatBbox {
        west: -180.0,
        south: -85.0,
        east: 180.0,
        north: 85.0,
    };
    let center = MapCenter {
        lon: 0.0,
        lat: 0.0,
        zoom: 0,
    };
    assert!(PmtilesConfig::new(3, 2, world, center).is_err());
    let inverted = LonLatBbox {
        west: 10.0,
        east: 5.0,
        ..world
    };
    assert!(PmtilesConfig::new(0, 2, inverted, center).is_err());
    let nan = MapCenter {
        lon: f64::NAN,
        ..center
    };
    assert!(PmtilesConfig::new(0, 2, world, nan).is_err());
}

fn add_monotonic_unique_tiles(writer: &mut PmtilesWriter, z: u8, count: usize) {
    let base = xy_to_tile_id(z, 0, 0);
    for i in 0..count {
        let tile_id = base + i as u64;
        let (z2, x, y) = tile_id_to_zxy(tile_id);
        assert_eq!(z2, z, "tile_id {tile_id} should decode back to z={z}");
        let payload = (i as u64).to_le_bytes();
        writer.add_tile(z2, x, y, &payload).unwrap();
    }
}

// -----------------------------------------------------------------------
// Varint round-trip
// -----------------------------------------------------------------------

#[test]
fn test_varint_roundtrip() {
    use protohoggr::Cursor;
    let test_values: &[u64] = &[0, 1, 127, 128, 255, 256, 300, 16383, 16384, u64::MAX];
    for &val in test_values {
        let mut buf = Vec::new();
        encode_varint(&mut buf, val);
        let mut c = Cursor::new(&buf);
        let decoded = c.read_varint().unwrap();
        assert_eq!(val, decoded, "varint roundtrip failed for {val}");
        assert!(c.is_empty(), "varint did not consume all bytes for {val}");
    }
}

#[test]
fn test_varint_known_encoding() {
    let mut buf = Vec::new();
    encode_varint(&mut buf, 5);
    assert_eq!(buf, vec![0x05]);

    buf.clear();
    encode_varint(&mut buf, 300);
    assert_eq!(buf, vec![0xAC, 0x02]);
}

// -----------------------------------------------------------------------
// Directory encoding
// -----------------------------------------------------------------------

#[test]
fn test_directory_encode_decode() {
    use protohoggr::Cursor;
    let entries = vec![
        WireEntry {
            tile_id: 5,
            offset: 0,
            length: 100,
            run_length: 3,
        },
        WireEntry {
            tile_id: 10,
            offset: 100,
            length: 50,
            run_length: 1,
        },
    ];

    let encoded = encode_directory(&entries).expect("encode directory");
    let mut c = Cursor::new(&encoded);

    let count = c.read_varint().unwrap();
    assert_eq!(count, 2);

    // Tile IDs (delta): first=5, delta=5
    let id0 = c.read_varint().unwrap();
    let id1 = c.read_varint().unwrap();
    assert_eq!(id0, 5);
    assert_eq!(id1, 5);

    // Run lengths: 3, 1
    let r0 = c.read_varint().unwrap();
    let r1 = c.read_varint().unwrap();
    assert_eq!(r0, 3);
    assert_eq!(r1, 1);

    // Lengths: 100, 50
    let l0 = c.read_varint().unwrap();
    let l1 = c.read_varint().unwrap();
    assert_eq!(l0, 100);
    assert_eq!(l1, 50);

    // Offsets: first=0+1=1, second=0 (contiguous: 0+100=100)
    let o0 = c.read_varint().unwrap();
    let o1 = c.read_varint().unwrap();
    assert_eq!(o0, 1);
    assert_eq!(o1, 0);
}

#[test]
fn test_directory_non_contiguous_offset() {
    use protohoggr::Cursor;
    let entries = vec![
        WireEntry {
            tile_id: 1,
            offset: 0,
            length: 100,
            run_length: 1,
        },
        WireEntry {
            tile_id: 5,
            offset: 500,
            length: 50,
            run_length: 1,
        },
    ];

    let encoded = encode_directory(&entries).expect("encode directory");
    let mut c = Cursor::new(&encoded);

    // Skip count, tile_ids, run_lengths, lengths
    let _count = c.read_varint().unwrap();
    let _id0 = c.read_varint().unwrap();
    let _id1 = c.read_varint().unwrap();
    let _r0 = c.read_varint().unwrap();
    let _r1 = c.read_varint().unwrap();
    let _l0 = c.read_varint().unwrap();
    let _l1 = c.read_varint().unwrap();

    let o0 = c.read_varint().unwrap();
    let o1 = c.read_varint().unwrap();
    assert_eq!(o0, 1); // offset 0 + 1
    assert_eq!(o1, 501); // offset 500 + 1 (not contiguous)
}

#[test]
fn directory_codec_roundtrip_property() {
    let mut state = 0xD1B5_4A32_D192_ED03_u64;

    for case in 0_usize..128 {
        let count = case % 48;
        let mut entries: Vec<WireEntry> = Vec::with_capacity(count);
        let mut tile_id = 0_u64;
        let mut offset;

        for index in 0..count {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            if index == 0 {
                tile_id = state % 5;
                offset = state % 100;
            } else {
                tile_id += state % 11 + 1;
                let previous = entries.last().expect("previous entry exists");
                let end = previous.offset + u64::from(previous.length);
                offset = if state & 1 == 0 {
                    end
                } else {
                    end + (state % 53) + 1
                };
            }

            let value = u32::try_from(state >> 32).expect("shifted random value fits u32");
            let length = value;
            let run_length = if state & 4 == 0 { 0 } else { value };
            entries.push(WireEntry {
                tile_id,
                offset,
                length,
                run_length,
            });
        }

        let encoded = encode_directory(&entries).expect("encode generated directory");
        let decoded = super::super::directory::decode_directory(&encoded)
            .expect("decode generated directory");
        assert_eq!(
            decoded, entries,
            "directory roundtrip failed in case {case}"
        );
    }
}

// -----------------------------------------------------------------------
// Dedup
// -----------------------------------------------------------------------

#[test]
fn test_dedup() {
    let config = PmtilesConfig::new(
        0,
        1,
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
    let data = gzip_compress(b"same-content").unwrap();

    let unique1 = writer.add_tile(0, 0, 0, &data).unwrap();
    let unique2 = writer.add_tile(1, 0, 0, &data).unwrap();

    assert!(unique1, "first tile should be unique");
    assert!(!unique2, "second tile should be deduplicated");
    assert_eq!(writer.unique_count, 1);
}

// -----------------------------------------------------------------------
// Metadata
// -----------------------------------------------------------------------

#[test]
fn test_metadata_json() {
    let config = PmtilesConfig::new(
        0,
        14,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 2,
        },
    )
    .expect("valid archive config");
    let json = build_metadata(
        &config,
        TileDataFormat::Mvt,
        TileDataCompression::Gzip,
        None,
        None,
        &MetadataExtensions::default(),
        &crate::shortbread::Layer::ALL,
    )
    .expect("serialize metadata");
    let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed["name"], "Shortbread");
    assert_eq!(parsed["format"], "pbf");
    assert_eq!(parsed["tile_payload_format"], "mvt");
    assert_eq!(parsed["tile_compression"], "gzip");
    let layers = parsed["vector_layers"].as_array().unwrap();
    assert_eq!(layers.len(), 26);
    assert_eq!(layers[0]["id"], "water_polygons");
    assert_eq!(layers[15]["id"], "streets");
    assert_eq!(layers[25]["id"], "ocean");
    assert!(parsed.get("source_pbf").is_none());
    assert!(parsed.get("osmosis_replication_timestamp").is_none());
}

#[test]
fn test_metadata_json_with_source_provenance() {
    let config = PmtilesConfig::new(
        0,
        14,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 2,
        },
    )
    .expect("valid archive config");
    let json = build_metadata(
        &config,
        TileDataFormat::Mvt,
        TileDataCompression::Gzip,
        Some("denmark-latest.osm.pbf"),
        Some(1_708_000_000),
        &MetadataExtensions::default(),
        &crate::shortbread::Layer::ALL,
    )
    .expect("serialize metadata");
    let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed["source_pbf"], "denmark-latest.osm.pbf");
    assert_eq!(parsed["osmosis_replication_timestamp"], 1_708_000_000);
}

#[test]
fn test_metadata_json_escapes_source_pbf_special_chars() {
    let config = PmtilesConfig::new(
        0,
        14,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 2,
        },
    )
    .expect("valid archive config");
    let source = "input\\\"name\nline\twith\rcontrols\\path.osm.pbf";
    let json = build_metadata(
        &config,
        TileDataFormat::Mvt,
        TileDataCompression::Gzip,
        Some(source),
        None,
        &MetadataExtensions::default(),
        &crate::shortbread::Layer::ALL,
    )
    .expect("serialize metadata");

    // Escaped JSON should parse and roundtrip to the exact original filename.
    let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed["source_pbf"], source);
}

#[test]
fn test_metadata_json_mlt_contract() {
    let config = PmtilesConfig::new(
        0,
        14,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 2,
        },
    )
    .expect("valid archive config");
    let json = build_metadata(
        &config,
        TileDataFormat::Mlt,
        TileDataCompression::None,
        None,
        None,
        &MetadataExtensions::default(),
        &crate::shortbread::Layer::ALL,
    )
    .expect("serialize metadata");
    let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed["format"], "mlt");
    assert_eq!(parsed["tile_payload_format"], "mlt");
    assert_eq!(parsed["tile_compression"], "none");
}

#[test]
fn metadata_serialization_keeps_the_existing_field_order() {
    let config = PmtilesConfig::new(
        0,
        14,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 2,
        },
    )
    .expect("valid archive config");
    let json = build_metadata(
        &config,
        TileDataFormat::Mvt,
        TileDataCompression::Gzip,
        Some("denmark-latest.osm.pbf"),
        Some(1_708_000_000),
        &MetadataExtensions::default(),
        &crate::shortbread::Layer::ALL,
    )
    .expect("serialize metadata");
    let json = std::str::from_utf8(&json).unwrap();

    assert!(json.starts_with(
        r#"{"name":"Shortbread","format":"pbf","tile_payload_format":"mvt","tile_compression":"gzip","type":"baselayer","minzoom":0,"maxzoom":14,"vector_layers":[{"id":"water_polygons","minzoom":4,"maxzoom":14}"#
    ));
    let source_position = json
        .find(r#","source_pbf":"denmark-latest.osm.pbf""#)
        .unwrap();
    let timestamp_position = json
        .find(r#","osmosis_replication_timestamp":1708000000"#)
        .unwrap();
    assert!(source_position < timestamp_position);
}

#[test]
fn metadata_setter_rejects_duplicates_and_non_objects() {
    let config = PmtilesConfig::new(
        0,
        14,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 2,
        },
    )
    .expect("valid archive config");
    let mut writer = PmtilesWriter::new(config);

    writer
        .set_elivagar_metadata(serde_json::json!({"schema": 1}))
        .expect("add first member");
    assert!(
        writer
            .set_elivagar_metadata(serde_json::json!({"schema": 2}))
            .is_err()
    );
    assert!(
        writer
            .set_elivagar_metadata(serde_json::Value::Null)
            .is_err()
    );

    let json = build_metadata(
        &writer.config,
        writer.tile_data_format,
        writer.tile_data_compression,
        None,
        None,
        &writer.metadata_extensions,
        &writer.metadata_layers,
    )
    .expect("serialize metadata");
    let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed["elivagar"]["schema"], 1);
}

// -----------------------------------------------------------------------
// Run-length encoding
// -----------------------------------------------------------------------

#[test]
fn test_run_length_dedup() {
    // PMTiles v3: run_length means all tiles in the run share the SAME data.
    // Consecutive dedup'd tiles pointing to the same offset should merge.
    let config = PmtilesConfig::new(
        1,
        1,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 1,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);

    // All 4 tiles at z=1 with the same data → dedup'd to same offset.
    let data = gzip_compress(b"same").unwrap();
    writer.add_tile(1, 0, 0, &data).unwrap();
    writer.add_tile(1, 0, 1, &data).unwrap();
    writer.add_tile(1, 1, 1, &data).unwrap();
    writer.add_tile(1, 1, 0, &data).unwrap();

    assert_eq!(writer.unique_count, 1);
    let entries = writer.collect_dir_entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].run_length, 4);
}

#[test]
fn test_no_run_for_different_data() {
    // Consecutive tiles with different data should NOT form runs,
    // even if they have the same compressed length.
    let config = PmtilesConfig::new(
        1,
        1,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 1,
        },
    )
    .expect("valid archive config");

    let mut writer = PmtilesWriter::new(config);
    // Add 4 tiles with distinct data - each gets a different offset, no runs possible
    let data_a = gzip_compress(b"data-a").unwrap();
    let data_b = gzip_compress(b"data-b").unwrap();
    let data_c = gzip_compress(b"data-c").unwrap();
    let data_d = gzip_compress(b"data-d").unwrap();
    writer.add_tile(1, 0, 0, &data_a).unwrap();
    writer.add_tile(1, 0, 1, &data_b).unwrap();
    writer.add_tile(1, 1, 1, &data_c).unwrap();
    writer.add_tile(1, 1, 0, &data_d).unwrap();

    let entries = writer.collect_dir_entries().unwrap();
    assert_eq!(entries.len(), 4);
}

// -----------------------------------------------------------------------
// End-to-end write_to
// -----------------------------------------------------------------------

#[test]
fn write_to_streaming_produces_valid_header() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let config = PmtilesConfig::new(
        0,
        1,
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

    let out_path = dir.path().join("test_streaming.pmtiles");
    let mut writer = PmtilesWriter::new_streaming(config, dir.path(), &out_path).unwrap();

    let tile_a = gzip_compress(b"tile-a").unwrap();
    let tile_b = gzip_compress(b"tile-b").unwrap();
    let tile_c = gzip_compress(b"tile-c").unwrap();

    writer.add_tile(0, 0, 0, &tile_a).unwrap();
    writer.add_tile(1, 0, 0, &tile_b).unwrap();
    writer.add_tile(1, 1, 0, &tile_c).unwrap();

    assert_eq!(writer.tile_count(), 3);
    assert_eq!(writer.unique_tile_count(), 3);

    writer.write_to(&out_path).unwrap();
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        names,
        vec![std::ffi::OsString::from("test_streaming.pmtiles")],
        "the partial file must be renamed away on completion"
    );

    let bytes = std::fs::read(&out_path).unwrap();
    assert!(bytes.len() > 127);
    assert_eq!(&bytes[0..7], b"PMTiles");
    assert_eq!(bytes[7], 3);
}

/// Publishing over an existing archive replaces the directory entry with a
/// new inode in both modes: a hard link to the old file keeps its bytes, which
/// is what keeps a memory map of the previous archive at that path sound. An
/// abandoned streaming writer leaves no partial behind.
#[test]
fn write_to_never_rewrites_an_existing_inode() {
    let dir = tempfile::tempdir().unwrap();
    let spill = tempfile::tempdir().unwrap();
    let config = || {
        PmtilesConfig::new(
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
        .expect("valid archive config")
    };
    let out_path = dir.path().join("out.pmtiles");
    let link_path = dir.path().join("link.pmtiles");
    let old = b"previous archive bytes".to_vec();
    std::fs::write(&out_path, &old).unwrap();
    std::fs::hard_link(&out_path, &link_path).unwrap();
    let tile = gzip_compress(b"tile").unwrap();

    let mut memory = PmtilesWriter::new(config());
    memory.add_tile(0, 0, 0, &tile).unwrap();
    memory.write_to(&out_path).unwrap();
    assert_eq!(std::fs::read(&link_path).unwrap(), old);

    std::fs::remove_file(&out_path).unwrap();
    std::fs::hard_link(&link_path, &out_path).unwrap();
    let mut streaming = PmtilesWriter::new_streaming(config(), spill.path(), &out_path).unwrap();
    streaming.add_tile(0, 0, 0, &tile).unwrap();
    streaming.write_to(&out_path).unwrap();
    assert_eq!(std::fs::read(&link_path).unwrap(), old);
    assert_ne!(std::fs::read(&out_path).unwrap(), old);

    let mut abandoned = PmtilesWriter::new_streaming(config(), spill.path(), &out_path).unwrap();
    abandoned.add_tile(0, 0, 0, &tile).unwrap();
    drop(abandoned);
    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["link.pmtiles", "out.pmtiles"]);
}

#[test]
fn write_to_streaming_matches_in_memory() {
    // Both modes should produce byte-identical archives for the same input.
    let dir = tempfile::tempdir().expect("create tempdir");
    let make_config = || {
        PmtilesConfig::new(
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
                zoom: 1,
            },
        )
        .expect("valid archive config")
    };

    let tile_a = gzip_compress(b"tile-a").unwrap();
    let tile_b = gzip_compress(b"tile-b").unwrap();
    let tile_c = gzip_compress(b"tile-c").unwrap();

    // In-memory writer
    let mut mem_writer = PmtilesWriter::new(make_config());
    mem_writer.add_tile(0, 0, 0, &tile_a).unwrap();
    mem_writer.add_tile(1, 0, 0, &tile_b).unwrap();
    mem_writer.add_tile(1, 1, 0, &tile_c).unwrap();
    let mem_path = dir.path().join("mem.pmtiles");
    mem_writer.write_to(&mem_path).unwrap();

    // Streaming writer
    let stream_path = dir.path().join("stream.pmtiles");
    let mut stream_writer =
        PmtilesWriter::new_streaming(make_config(), dir.path(), &stream_path).unwrap();
    stream_writer.add_tile(0, 0, 0, &tile_a).unwrap();
    stream_writer.add_tile(1, 0, 0, &tile_b).unwrap();
    stream_writer.add_tile(1, 1, 0, &tile_c).unwrap();
    stream_writer.write_to(&stream_path).unwrap();

    let mem_bytes = std::fs::read(&mem_path).unwrap();
    let stream_bytes = std::fs::read(&stream_path).unwrap();
    assert_eq!(
        mem_bytes, stream_bytes,
        "streaming and in-memory archives should be byte-identical"
    );
}

#[test]
fn write_to_produces_valid_header() {
    let config = PmtilesConfig::new(
        0,
        1,
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

    let tile_a = gzip_compress(b"tile-a").unwrap();
    let tile_b = gzip_compress(b"tile-b").unwrap();
    let tile_c = gzip_compress(b"tile-c").unwrap();

    writer.add_tile(0, 0, 0, &tile_a).unwrap();
    writer.add_tile(1, 0, 0, &tile_b).unwrap();
    writer.add_tile(1, 1, 0, &tile_c).unwrap();

    assert_eq!(writer.tile_count(), 3);
    assert_eq!(writer.unique_tile_count(), 3);

    // Write to a temporary file and verify the header.
    let dir = tempfile::tempdir().expect("create tempdir");
    let out_path = dir.path().join("test_write_to.pmtiles");

    writer.write_to(&out_path).unwrap();

    let bytes = std::fs::read(&out_path).unwrap();
    // PMTiles v3 header is 127 bytes
    assert!(
        bytes.len() > 127,
        "output file should be larger than the 127-byte header, got {} bytes",
        bytes.len(),
    );
    // First 7 bytes: "PMTiles"
    assert_eq!(&bytes[0..7], b"PMTiles");
    // Byte 7: version = 3
    assert_eq!(bytes[7], 3);
}

#[test]
fn write_to_respects_tile_contract_in_header() {
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
    writer.set_tile_contract(TileDataFormat::Mlt, TileDataCompression::None);
    writer.add_tile(0, 0, 0, b"raw-mlt-payload").unwrap();

    let dir = tempfile::tempdir().expect("create tempdir");
    let out_path = dir.path().join("test_contract_header.pmtiles");
    writer.write_to(&out_path).unwrap();
    let bytes = std::fs::read(&out_path).unwrap();

    let header = header_of(&bytes);
    assert_eq!(header.tile_compression, HeaderCompression::None);
    assert_eq!(
        header.tile_type,
        TileType::Unknown,
        "tile_type should be unknown for mlt payload"
    );
}

// -----------------------------------------------------------------------
// Dedup metrics
// -----------------------------------------------------------------------

#[test]
fn test_dedup_metrics_basic() {
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
    let data_a = gzip_compress(b"content-a").unwrap();
    let data_b = gzip_compress(b"content-b").unwrap();

    // First tile: unique, no candidates.
    writer.add_tile(0, 0, 0, &data_a).unwrap();
    assert_eq!(writer.dedup_stats.candidates, 0);
    assert_eq!(writer.dedup_stats.tiles_reused, 0);

    // Second tile: same content → dedup.
    writer.add_tile(1, 0, 0, &data_a).unwrap();
    assert_eq!(writer.dedup_stats.candidates, 1);
    assert_eq!(writer.dedup_stats.tiles_reused, 1);
    assert_eq!(writer.dedup_stats.bytes_saved, data_a.len() as u64);

    // Third tile: different content → no candidate hit (different hash1).
    writer.add_tile(1, 0, 1, &data_b).unwrap();
    assert_eq!(writer.dedup_stats.candidates, 1); // unchanged
    assert_eq!(writer.dedup_stats.tiles_reused, 1); // unchanged

    // Fourth tile: same as first again → another dedup.
    writer.add_tile(1, 1, 1, &data_a).unwrap();
    assert_eq!(writer.dedup_stats.candidates, 2);
    assert_eq!(writer.dedup_stats.tiles_reused, 2);
    assert_eq!(writer.dedup_stats.bytes_saved, 2 * data_a.len() as u64);

    assert_eq!(writer.unique_count, 2);
}

#[test]
fn add_run_records_one_blob_and_all_addressed_tiles() {
    let config = PmtilesConfig::new(
        0,
        14,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 2,
        },
    )
    .expect("valid archive config");
    let mut writer = PmtilesWriter::new(config);
    let base = xy_to_tile_id(3, 0, 0);
    assert!(writer.add_run(tid(base), 4, b"ocean").expect("add run"));
    assert_eq!(writer.num_addressed, 4);
    assert_eq!(writer.unique_tile_count(), 1);
    let entries = writer.collect_dir_entries().expect("collect directory");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].run_length, 4);
}

#[test]
fn add_tile_rejects_non_increasing_ids_without_mutating_writer() {
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
    writer.add_tile(1, 0, 0, b"shared").expect("first tile");

    let error = writer
        .add_tile(1, 0, 0, b"duplicate")
        .expect_err("a duplicate tile ID must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    let error = writer
        .add_tile(0, 0, 0, b"out of order")
        .expect_err("a lower tile ID must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(writer.tile_count(), 1);
    assert_eq!(writer.unique_tile_count(), 1);

    assert!(
        !writer
            .add_tile(1, 0, 1, b"shared")
            .expect("next tile after rejected input")
    );
    let entries = writer.collect_dir_entries().expect("directory entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].run_length, 2);
}

#[test]
fn add_tile_rejects_invalid_coordinates_without_mutating_writer() {
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

    let error = writer
        .add_tile(2, 4, 0, b"invalid x")
        .expect_err("x outside the zoom grid must be refused");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(writer.tile_count(), 0);
    assert_eq!(writer.unique_tile_count(), 0);
}

#[test]
fn add_run_rejects_overlapping_ids_without_mutating_writer() {
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
    let base = xy_to_tile_id(1, 0, 0);
    writer.add_run(tid(base), 2, b"shared").expect("first run");

    let error = writer
        .add_run(tid(base + 1), 1, b"overlap")
        .expect_err("a run overlapping the previous run must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(writer.tile_count(), 2);
    assert_eq!(writer.unique_tile_count(), 1);

    assert!(
        !writer
            .add_run(tid(base + 2), 1, b"shared")
            .expect("next run after rejected input")
    );
    let entries = writer.collect_dir_entries().expect("directory entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].run_length, 3);
}

#[test]
fn add_run_rejects_tile_id_overflow_without_mutating_writer() {
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

    // The last z31 id is addressable, so a one-tile run there is legal; a
    // run reaching past it is not.
    let last = crate::tiling::ARCHIVE_ID_END - 1;
    for (start, run_length) in [(last, 2), (last - 1, 3), (last, u32::MAX)] {
        let error = writer
            .add_run(tid(start), run_length, b"overflow")
            .expect_err("a run must end within the addressable tile ids");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
    assert_eq!(writer.tile_count(), 0);
    assert_eq!(writer.unique_tile_count(), 0);
}

#[test]
fn add_run_counts_dedup_candidate_rejections() {
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
    let data = gzip_compress(b"run fingerprint").expect("gzip payload");
    let (hash, fingerprint) = PmtilesWriter::compute_dedup_hashes(&data);
    let length = u32::try_from(data.len()).expect("compressed payload length fits");
    writer.inject_dedup_entry(hash, 0, length + 1, fingerprint);
    writer.inject_dedup_entry(hash, 0, length, fingerprint ^ 1);

    assert!(
        writer
            .add_run(tid(xy_to_tile_id(1, 0, 0)), 2, &data)
            .expect("add run")
    );
    assert_eq!(writer.dedup_stats.candidates, 1);
    assert_eq!(writer.dedup_stats.reject_len_mismatch, 1);
    assert_eq!(writer.dedup_stats.reject_fp_mismatch, 1);
    assert_eq!(writer.dedup_stats.tiles_reused, 1);
}

#[test]
fn add_run_matches_expanded_add_tile_below_dedup_cap() {
    let run_config = PmtilesConfig::new(
        0,
        14,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 2,
        },
    )
    .expect("valid archive config");
    let tile_config = PmtilesConfig::new(
        0,
        14,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 2,
        },
    )
    .expect("valid archive config");
    let base = xy_to_tile_id(3, 0, 0);
    let payload = b"ocean";
    let mut run_writer = PmtilesWriter::new(run_config);
    let mut tile_writer = PmtilesWriter::new(tile_config);
    assert!(run_writer.add_run(tid(base), 4, payload).expect("add run"));
    for tile_id in base..base + 4 {
        let (z, x, y) = tile_id_to_zxy(tile_id);
        let _ = tile_writer.add_tile(z, x, y, payload).expect("add tile");
    }
    let run_entries = run_writer.collect_dir_entries().expect("run entries");
    let tile_entries = tile_writer.collect_dir_entries().expect("tile entries");
    assert_eq!(run_writer.num_addressed, tile_writer.num_addressed);
    assert_eq!(run_writer.unique_count, tile_writer.unique_count);
    assert_eq!(run_entries.len(), tile_entries.len());
    assert_eq!(run_entries[0].tile_id, tile_entries[0].tile_id);
    assert_eq!(run_entries[0].offset, tile_entries[0].offset);
    assert_eq!(run_entries[0].length, tile_entries[0].length);
    assert_eq!(run_entries[0].run_length, tile_entries[0].run_length);
}

#[test]
fn add_run_preserves_run_sharing_after_the_dedup_cap() {
    let run_config = PmtilesConfig::new(
        0,
        14,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 2,
        },
    )
    .expect("valid archive config");
    let tile_config = PmtilesConfig::new(
        0,
        14,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 2,
        },
    )
    .expect("valid archive config");
    let base = xy_to_tile_id(3, 0, 0);
    let payload = b"ocean";
    let mut run_writer = PmtilesWriter::new(run_config);
    let mut tile_writer = PmtilesWriter::new(tile_config);
    run_writer.set_dedup_cap(0);
    tile_writer.set_dedup_cap(0);
    assert!(run_writer.add_run(tid(base), 4, payload).expect("add run"));
    for tile_id in base..base + 4 {
        let (z, x, y) = tile_id_to_zxy(tile_id);
        assert!(tile_writer.add_tile(z, x, y, payload).expect("add tile"));
    }
    assert_eq!(run_writer.num_addressed, tile_writer.num_addressed);
    assert_eq!(run_writer.unique_count, 1);
    assert_eq!(tile_writer.unique_count, 4);
    assert_eq!(
        run_writer.collect_dir_entries().expect("run entries").len(),
        1
    );
    assert_eq!(
        tile_writer
            .collect_dir_entries()
            .expect("tile entries")
            .len(),
        4
    );
}

#[test]
fn test_dedup_cap_overflow() {
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
    writer.set_dedup_cap(2);

    // Add 3 unique tiles. First 2 fit in the cap, third is skipped.
    let data_a = gzip_compress(b"aaa").unwrap();
    let data_b = gzip_compress(b"bbb").unwrap();
    let data_c = gzip_compress(b"ccc").unwrap();

    writer.add_tile(0, 0, 0, &data_a).unwrap(); // inserted
    writer.add_tile(1, 0, 0, &data_b).unwrap(); // inserted
    writer.add_tile(1, 0, 1, &data_c).unwrap(); // skipped (cap=2)

    assert_eq!(writer.dedup_stats.insert_skipped_cap, 1);
    assert_eq!(writer.dedup.entry_count(), 2);

    // Copy of tile A → should still dedup (it's in the map).
    let dup_a = writer.add_tile(1, 1, 1, &data_a).unwrap();
    assert!(!dup_a, "tile A copy should dedup");
    assert_eq!(writer.dedup_stats.tiles_reused, 1);

    // Copy of tile C → should NOT dedup (was never inserted).
    let dup_c = writer.add_tile(1, 1, 0, &data_c).unwrap();
    assert!(dup_c, "tile C copy should be unique (not in map)");
    assert_eq!(writer.dedup_stats.insert_skipped_cap, 2); // another skip
}

#[test]
fn test_dedup_fingerprint_rejection() {
    let config = PmtilesConfig::new(
        0,
        1,
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

    // Compute real hashes for a tile we'll add.
    let data = gzip_compress(b"real-tile").unwrap();
    let (hash1, _real_fp2) = PmtilesWriter::compute_dedup_hashes(&data);

    // Inject a fake entry under the same hash1 but with a wrong fingerprint
    // and matching length - simulates a primary hash collision.
    let fake_fp2 = 0xDEAD_BEEF_CAFE_BABE;
    #[allow(clippy::cast_possible_truncation)]
    writer.inject_dedup_entry(hash1, 0, data.len() as u32, fake_fp2);

    // Now add the real tile - hash1 matches, length matches, but fp2 differs.
    let is_unique = writer.add_tile(0, 0, 0, &data).unwrap();
    assert!(is_unique, "should NOT dedup when fingerprint differs");
    assert_eq!(writer.dedup_stats.candidates, 1);
    assert_eq!(writer.dedup_stats.reject_fp_mismatch, 1);
    assert_eq!(writer.dedup_stats.tiles_reused, 0);
}

#[test]
fn test_dedup_bucket_collision() {
    let config = PmtilesConfig::new(
        0,
        1,
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

    let data_a = gzip_compress(b"tile-a-bucket").unwrap();
    let (hash1_a, _fp2_a) = PmtilesWriter::compute_dedup_hashes(&data_a);

    // Add tile A normally.
    writer.add_tile(0, 0, 0, &data_a).unwrap();
    assert_eq!(writer.dedup_stats.hash_bucket_collisions, 0);

    // Inject a second entry under the same hash1 (different tile).
    writer.inject_dedup_entry(hash1_a, 999, 42, 0x1234);
    // The inject itself won't increment the collision counter - that only
    // happens during add_tile insertion. But we can verify the bucket has 2 entries.

    // Add tile A again - should find the correct match (first entry in bucket).
    let dup = writer.add_tile(1, 0, 0, &data_a).unwrap();
    assert!(!dup, "should dedup with correct bucket entry");
    assert_eq!(writer.dedup_stats.tiles_reused, 1);

    // The lookup should have seen 1 candidate (the bucket had entries).
    assert_eq!(writer.dedup_stats.candidates, 1);

    // Verify length mismatch was counted for the injected entry (len=42 vs data_a.len()).
    // The scan checks the injected entry first or second depending on order.
    // One entry matches (tile A), the other has len=42 → reject_len_mismatch.
    let total_rejects =
        writer.dedup_stats.reject_len_mismatch + writer.dedup_stats.reject_fp_mismatch;
    // We matched on one entry but may have checked the other first.
    // The exact count depends on iteration order, but total_rejects should be 0 or 1.
    assert!(total_rejects <= 1);

    // Now verify a tile that hits the bucket but matches only the injected entry's
    // hash1 (not fp2) - should be unique.
    // We can't easily manufacture this, so just verify the counters are sane.
    assert_eq!(writer.dedup_stats.bytes_saved, data_a.len() as u64);
}

#[test]
fn test_dedup_len_mismatch_counter() {
    let config = PmtilesConfig::new(
        0,
        1,
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

    // Compute hashes for a tile and inject an entry with same hash1 but different length.
    let data = gzip_compress(b"len-test").unwrap();
    let (hash1, fp2) = PmtilesWriter::compute_dedup_hashes(&data);

    // Inject entry with same hash1/fp2 but wrong length.
    writer.inject_dedup_entry(hash1, 0, 999, fp2);

    let is_unique = writer.add_tile(0, 0, 0, &data).unwrap();
    assert!(is_unique, "should NOT dedup when length differs");
    assert_eq!(writer.dedup_stats.candidates, 1);
    assert_eq!(writer.dedup_stats.reject_len_mismatch, 1);
    assert_eq!(writer.dedup_stats.reject_fp_mismatch, 0);
}

// -----------------------------------------------------------------------
// Data section 4K alignment (always-on for O_DIRECT serving)
// -----------------------------------------------------------------------

#[test]
fn data_section_is_4k_aligned() {
    let config = PmtilesConfig::new(
        0,
        1,
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
    let tile_a = gzip_compress(b"tile-a").unwrap();
    let tile_b = gzip_compress(b"tile-b").unwrap();
    writer.add_tile(0, 0, 0, &tile_a).unwrap();
    writer.add_tile(1, 0, 0, &tile_b).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aligned.pmtiles");
    writer.write_to(&path).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let data_offset = header_of(&bytes).tile_data.offset;
    assert_eq!(
        data_offset % 4096,
        0,
        "data_offset {data_offset} should be 4K-aligned"
    );
}

#[test]
fn root_only_layout_offsets_are_consistent() {
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
    writer.add_tile(0, 0, 0, &[1, 2, 3, 4]).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("root_only.pmtiles");
    writer.write_to(&path).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 127, "archive should be larger than header");

    let header = header_of(&bytes);
    let Section {
        offset: root_dir_offset,
        length: root_dir_length,
    } = header.root_dir;
    let Section {
        offset: metadata_offset,
        length: metadata_length,
    } = header.metadata;
    let Section {
        offset: leaf_dirs_offset,
        length: leaf_dirs_length,
    } = header.leaf_dirs;
    let Section {
        offset: data_offset,
        length: data_length,
    } = header.tile_data;

    // Layout: [header][root][zeros to 16384][tile data][metadata][leaf dirs].
    assert_eq!(root_dir_offset, 127);
    assert!(root_dir_length > 0);
    assert!(
        root_dir_offset + root_dir_length <= 16384,
        "root directory must live within the spec's first 16,384 bytes"
    );
    assert_eq!(data_offset, 16384);
    assert_eq!(metadata_offset, data_offset + data_length);
    assert!(metadata_length > 0);
    assert_eq!(leaf_dirs_offset, metadata_offset + metadata_length);
    assert_eq!(
        leaf_dirs_length, 0,
        "single-tile archive should be root-only"
    );

    let file_len = bytes.len() as u64;
    assert!(root_dir_offset + root_dir_length <= file_len);
    assert!(metadata_offset + metadata_length <= file_len);
    assert!(data_offset + data_length <= file_len);
    assert_eq!(
        file_len,
        leaf_dirs_offset + leaf_dirs_length,
        "leaf dirs are the last section"
    );
}

#[test]
fn root_leaf_boundary_switches_at_threshold() {
    // finalize_directories() uses MAX_ROOT_ENTRIES=16384. The monotonic
    // helper's entries are maximally regular (unit tile-id deltas, constant
    // lengths, contiguous offsets), so their compressed root stays far under
    // the init-section byte budget and the entry-count threshold is the
    // deciding rule for them.
    const ROOT_THRESHOLD: usize = 16384;
    let make_config = || {
        PmtilesConfig::new(
            8,
            8,
            LonLatBbox {
                west: -180.0,
                south: -85.0,
                east: 180.0,
                north: 85.0,
            },
            MapCenter {
                lon: 0.0,
                lat: 0.0,
                zoom: 8,
            },
        )
        .expect("valid archive config")
    };

    let dir = tempfile::tempdir().unwrap();

    let mut at_threshold = PmtilesWriter::new(make_config());
    add_monotonic_unique_tiles(&mut at_threshold, 8, ROOT_THRESHOLD);
    let at_threshold_path = dir.path().join("root_threshold.pmtiles");
    at_threshold.write_to(&at_threshold_path).unwrap();
    let at_threshold_bytes = std::fs::read(&at_threshold_path).unwrap();

    let mut over_threshold = PmtilesWriter::new(make_config());
    add_monotonic_unique_tiles(&mut over_threshold, 8, ROOT_THRESHOLD + 1);
    let over_threshold_path = dir.path().join("leaf_threshold.pmtiles");
    over_threshold.write_to(&over_threshold_path).unwrap();
    let over_threshold_bytes = std::fs::read(&over_threshold_path).unwrap();

    let threshold_num_entries = header_of(&at_threshold_bytes).tile_entries;
    let threshold_leaf_dirs_length = header_of(&at_threshold_bytes).leaf_dirs.length;
    assert_eq!(threshold_num_entries, ROOT_THRESHOLD as u64);
    assert_eq!(
        threshold_leaf_dirs_length, 0,
        "threshold case should remain root-only"
    );

    let over_num_entries = header_of(&over_threshold_bytes).tile_entries;
    let over_leaf_dirs_length = header_of(&over_threshold_bytes).leaf_dirs.length;
    assert_eq!(over_num_entries, (ROOT_THRESHOLD + 1) as u64);
    assert!(
        over_leaf_dirs_length > 0,
        "threshold+1 case should use leaf directories"
    );
}

#[test]
fn oversized_root_demotes_to_leaf_directories() {
    // The fixed data offset turns the spec's first-16,384-bytes root window
    // into a hard budget, so the root-vs-leaf decision is byte-driven on top
    // of the entry-count threshold: entries under MAX_ROOT_ENTRIES whose
    // compressed root overflows the init section must demote to leaf
    // directories, with the leaf fanout doubling until the root fits. Build
    // entries that resist compression - irregular tile-id gaps and varying
    // payload lengths from a deterministic LCG - so 16,000 of them compress
    // to a root far over 16 KiB.
    let config = PmtilesConfig::new(
        14,
        14,
        LonLatBbox {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 14,
        },
    )
    .expect("valid archive config");
    let mut writer = PmtilesWriter::new(config);

    let mut rng: u64 = 0x243F_6A88_85A3_08D3;
    let mut step = || {
        rng = rng
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        rng >> 33
    };
    let mut tile_id = xy_to_tile_id(14, 0, 0);
    let mut payload = Vec::new();
    for i in 0_u64..16_000 {
        tile_id += 1 + step() % 1000;
        let (z, x, y) = tile_id_to_zxy(tile_id);
        assert_eq!(z, 14);
        let len = 9 + usize::try_from(step() % 192).expect("fits usize");
        payload.clear();
        payload.extend_from_slice(&i.to_le_bytes());
        payload.resize(len, 0x5A);
        writer.add_tile(z, x, y, &payload).unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized_root.pmtiles");
    writer.write_to(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();

    let header = header_of(&bytes);
    let root_dir_offset = header.root_dir.offset;
    let root_dir_length = header.root_dir.length;
    let leaf_dirs_length = header.leaf_dirs.length;
    let num_entries = header.tile_entries;

    assert_eq!(num_entries, 16_000);
    assert_eq!(root_dir_offset, 127);
    assert!(
        leaf_dirs_length > 0,
        "a root over the init-section budget must demote to leaf directories"
    );
    assert!(
        root_dir_offset + root_dir_length <= 16384,
        "root ({root_dir_length} B) must fit the spec's first 16,384 bytes"
    );
}
