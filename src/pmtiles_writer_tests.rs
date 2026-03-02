use super::*;

// -----------------------------------------------------------------------
// Hilbert round-trip
// -----------------------------------------------------------------------

#[test]
fn test_hilbert_z0() {
    assert_eq!(xy_to_tile_id(0, 0, 0), 0);
    assert_eq!(tile_id_to_zxy(0), (0, 0, 0));
}

#[test]
fn test_hilbert_z1_known_values() {
    // z=1: base = (4-1)/3 = 1, 4 tiles at IDs 1..4
    assert_eq!(xy_to_tile_id(1, 0, 0), 1);
    assert_eq!(xy_to_tile_id(1, 0, 1), 2);
    assert_eq!(xy_to_tile_id(1, 1, 1), 3);
    assert_eq!(xy_to_tile_id(1, 1, 0), 4);
}

#[test]
fn test_hilbert_z2_base() {
    // z=2: base = (16-1)/3 = 5
    assert_eq!(xy_to_tile_id(2, 0, 0), 5);
}

#[test]
fn test_hilbert_roundtrip_z1() {
    for x in 0..2u32 {
        for y in 0..2u32 {
            let id = xy_to_tile_id(1, x, y);
            let (z2, x2, y2) = tile_id_to_zxy(id);
            assert_eq!(
                (1u8, x, y),
                (z2, x2, y2),
                "roundtrip failed for z=1,x={x},y={y}"
            );
        }
    }
}

#[test]
fn test_hilbert_roundtrip_z2() {
    for x in 0..4u32 {
        for y in 0..4u32 {
            let id = xy_to_tile_id(2, x, y);
            let (z2, x2, y2) = tile_id_to_zxy(id);
            assert_eq!(
                (2u8, x, y),
                (z2, x2, y2),
                "roundtrip failed for z=2,x={x},y={y}"
            );
        }
    }
}

#[test]
fn test_hilbert_roundtrip_z5() {
    let n = 1u32 << 5;
    for x in 0..n {
        for y in 0..n {
            let id = xy_to_tile_id(5, x, y);
            let (z2, x2, y2) = tile_id_to_zxy(id);
            assert_eq!(
                (5u8, x, y),
                (z2, x2, y2),
                "roundtrip failed for z=5,x={x},y={y}"
            );
        }
    }
}

#[test]
fn test_hilbert_roundtrip_z10_sample() {
    for x in (0..1024u32).step_by(37) {
        for y in (0..1024u32).step_by(41) {
            let id = xy_to_tile_id(10, x, y);
            let (z2, x2, y2) = tile_id_to_zxy(id);
            assert_eq!(
                (10u8, x, y),
                (z2, x2, y2),
                "roundtrip failed for z=10,x={x},y={y}"
            );
        }
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
        DirEntry {
            tile_id: 5,
            offset: 0,
            length: 100,
            run_length: 3,
        },
        DirEntry {
            tile_id: 10,
            offset: 100,
            length: 50,
            run_length: 1,
        },
    ];

    let encoded = encode_directory(&entries);
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
        DirEntry {
            tile_id: 1,
            offset: 0,
            length: 100,
            run_length: 1,
        },
        DirEntry {
            tile_id: 5,
            offset: 500,
            length: 50,
            run_length: 1,
        },
    ];

    let encoded = encode_directory(&entries);
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

// -----------------------------------------------------------------------
// Dedup
// -----------------------------------------------------------------------

#[test]
fn test_dedup() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 1,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),

    };

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
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 14,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 2),

    };
    let json = build_metadata(&config);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["name"], "Shortbread");
    assert_eq!(parsed["format"], "pbf");
    let layers = parsed["vector_layers"].as_array().unwrap();
    assert_eq!(layers.len(), 26);
    assert_eq!(layers[0]["id"], "water_polygons");
    assert_eq!(layers[15]["id"], "streets");
    assert_eq!(layers[25]["id"], "ocean");
}

// -----------------------------------------------------------------------
// Run-length encoding
// -----------------------------------------------------------------------

#[test]
fn test_run_length_dedup() {
    // PMTiles v3: run_length means all tiles in the run share the SAME data.
    // Consecutive dedup'd tiles pointing to the same offset should merge.
    let config = PmtilesConfig {
        min_zoom: 1,
        max_zoom: 1,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 1),

    };

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
    let config = PmtilesConfig {
        min_zoom: 1,
        max_zoom: 1,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 1),

    };

    let mut writer = PmtilesWriter::new(config);
    // Add 4 tiles with distinct data — each gets a different offset, no runs possible
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
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 1,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),

    };

    let mut writer = PmtilesWriter::new_streaming(config, dir.path()).unwrap();

    let tile_a = gzip_compress(b"tile-a").unwrap();
    let tile_b = gzip_compress(b"tile-b").unwrap();
    let tile_c = gzip_compress(b"tile-c").unwrap();

    writer.add_tile(0, 0, 0, &tile_a).unwrap();
    writer.add_tile(1, 0, 0, &tile_b).unwrap();
    writer.add_tile(1, 1, 0, &tile_c).unwrap();

    assert_eq!(writer.tile_count(), 3);
    assert_eq!(writer.unique_tile_count(), 3);

    let out_path = dir.path().join("test_streaming.pmtiles");
    writer.write_to(&out_path).unwrap();

    let bytes = std::fs::read(&out_path).unwrap();
    assert!(bytes.len() > 127);
    assert_eq!(&bytes[0..7], b"PMTiles");
    assert_eq!(bytes[7], 3);
}

#[test]
fn write_to_streaming_matches_in_memory() {
    // Both modes should produce byte-identical archives for the same input.
    let dir = tempfile::tempdir().expect("create tempdir");
    let make_config = || PmtilesConfig {
        min_zoom: 0,
        max_zoom: 2,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 1),

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
    let mut stream_writer = PmtilesWriter::new_streaming(make_config(), dir.path()).unwrap();
    stream_writer.add_tile(0, 0, 0, &tile_a).unwrap();
    stream_writer.add_tile(1, 0, 0, &tile_b).unwrap();
    stream_writer.add_tile(1, 1, 0, &tile_c).unwrap();
    let stream_path = dir.path().join("stream.pmtiles");
    stream_writer.write_to(&stream_path).unwrap();

    let mem_bytes = std::fs::read(&mem_path).unwrap();
    let stream_bytes = std::fs::read(&stream_path).unwrap();
    assert_eq!(mem_bytes, stream_bytes, "streaming and in-memory archives should be byte-identical");
}

#[test]
fn write_to_produces_valid_header() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 1,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),

    };

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

// -----------------------------------------------------------------------
// Data section 4K alignment (always-on for O_DIRECT serving)
// -----------------------------------------------------------------------

#[test]
fn data_section_is_4k_aligned() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 1,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

    let mut writer = PmtilesWriter::new(config);
    let tile_a = gzip_compress(b"tile-a").unwrap();
    let tile_b = gzip_compress(b"tile-b").unwrap();
    writer.add_tile(0, 0, 0, &tile_a).unwrap();
    writer.add_tile(1, 0, 0, &tile_b).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aligned.pmtiles");
    writer.write_to(&path).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let data_offset = u64::from_le_bytes(bytes[56..64].try_into().unwrap());
    assert_eq!(data_offset % 4096, 0, "data_offset {data_offset} should be 4K-aligned");
}
