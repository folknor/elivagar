use super::*;

fn read_u64_le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
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
    let json = build_metadata(
        &config,
        TileDataFormat::Mvt,
        TileDataCompression::Gzip,
        None,
        None,
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
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
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 14,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 2),
    };
    let json = build_metadata(
        &config,
        TileDataFormat::Mvt,
        TileDataCompression::Gzip,
        Some("denmark-latest.osm.pbf"),
        Some(1_708_000_000),
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["source_pbf"], "denmark-latest.osm.pbf");
    assert_eq!(parsed["osmosis_replication_timestamp"], 1_708_000_000);
}

#[test]
fn test_metadata_json_escapes_source_pbf_special_chars() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 14,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 2),
    };
    let source = "input\\\"name\nline\twith\rcontrols\\path.osm.pbf";
    let json = build_metadata(
        &config,
        TileDataFormat::Mvt,
        TileDataCompression::Gzip,
        Some(source),
        None,
    );

    // Escaped JSON should parse and roundtrip to the exact original filename.
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["source_pbf"], source);
}

#[test]
fn test_metadata_json_mlt_contract() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 14,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 2),
    };
    let json = build_metadata(
        &config,
        TileDataFormat::Mlt,
        TileDataCompression::None,
        None,
        None,
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["format"], "mlt");
    assert_eq!(parsed["tile_payload_format"], "mlt");
    assert_eq!(parsed["tile_compression"], "none");
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

#[test]
fn write_to_respects_tile_contract_in_header() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 0,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };
    let mut writer = PmtilesWriter::new(config);
    writer.set_tile_contract(TileDataFormat::Mlt, TileDataCompression::None);
    writer.add_tile(0, 0, 0, b"raw-mlt-payload").unwrap();

    let dir = tempfile::tempdir().expect("create tempdir");
    let out_path = dir.path().join("test_contract_header.pmtiles");
    writer.write_to(&out_path).unwrap();
    let bytes = std::fs::read(&out_path).unwrap();

    assert_eq!(bytes[98], 1, "tile_compression should be none");
    assert_eq!(bytes[99], 0, "tile_type should be unknown for mlt payload");
}

// -----------------------------------------------------------------------
// Dedup metrics
// -----------------------------------------------------------------------

#[test]
fn test_dedup_metrics_basic() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 2,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

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
fn test_dedup_cap_overflow() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 5,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

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
    assert_eq!(writer.dedup_count, 2);

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
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 1,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

    let mut writer = PmtilesWriter::new(config);

    // Compute real hashes for a tile we'll add.
    let data = gzip_compress(b"real-tile").unwrap();
    let (hash1, _real_fp2) = PmtilesWriter::compute_dedup_hashes(&data);

    // Inject a fake entry under the same hash1 but with a wrong fingerprint
    // and matching length — simulates a primary hash collision.
    let fake_fp2 = 0xDEAD_BEEF_CAFE_BABE;
    #[allow(clippy::cast_possible_truncation)]
    writer.inject_dedup_entry(hash1, 0, data.len() as u32, fake_fp2);

    // Now add the real tile — hash1 matches, length matches, but fp2 differs.
    let is_unique = writer.add_tile(0, 0, 0, &data).unwrap();
    assert!(is_unique, "should NOT dedup when fingerprint differs");
    assert_eq!(writer.dedup_stats.candidates, 1);
    assert_eq!(writer.dedup_stats.reject_fp_mismatch, 1);
    assert_eq!(writer.dedup_stats.tiles_reused, 0);
}

#[test]
fn test_dedup_bucket_collision() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 1,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

    let mut writer = PmtilesWriter::new(config);

    let data_a = gzip_compress(b"tile-a-bucket").unwrap();
    let (hash1_a, _fp2_a) = PmtilesWriter::compute_dedup_hashes(&data_a);

    // Add tile A normally.
    writer.add_tile(0, 0, 0, &data_a).unwrap();
    assert_eq!(writer.dedup_stats.hash_bucket_collisions, 0);

    // Inject a second entry under the same hash1 (different tile).
    writer.inject_dedup_entry(hash1_a, 999, 42, 0x1234);
    // The inject itself won't increment the collision counter — that only
    // happens during add_tile insertion. But we can verify the bucket has 2 entries.

    // Add tile A again — should find the correct match (first entry in bucket).
    let dup = writer.add_tile(1, 0, 0, &data_a).unwrap();
    assert!(!dup, "should dedup with correct bucket entry");
    assert_eq!(writer.dedup_stats.tiles_reused, 1);

    // The lookup should have seen 1 candidate (the bucket had entries).
    assert_eq!(writer.dedup_stats.candidates, 1);

    // Verify length mismatch was counted for the injected entry (len=42 vs data_a.len()).
    // The scan checks the injected entry first or second depending on order.
    // One entry matches (tile A), the other has len=42 → reject_len_mismatch.
    let total_rejects = writer.dedup_stats.reject_len_mismatch + writer.dedup_stats.reject_fp_mismatch;
    // We matched on one entry but may have checked the other first.
    // The exact count depends on iteration order, but total_rejects should be 0 or 1.
    assert!(total_rejects <= 1);

    // Now verify a tile that hits the bucket but matches only the injected entry's
    // hash1 (not fp2) — should be unique.
    // We can't easily manufacture this, so just verify the counters are sane.
    assert_eq!(writer.dedup_stats.bytes_saved, data_a.len() as u64);
}

#[test]
fn test_dedup_len_mismatch_counter() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 1,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };

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

#[test]
fn root_only_layout_offsets_are_consistent() {
    let config = PmtilesConfig {
        min_zoom: 0,
        max_zoom: 0,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 0),
    };
    let mut writer = PmtilesWriter::new(config);
    writer.add_tile(0, 0, 0, &[1, 2, 3, 4]).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("root_only.pmtiles");
    writer.write_to(&path).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 127, "archive should be larger than header");

    let root_dir_offset = read_u64_le(&bytes, 8);
    let root_dir_length = read_u64_le(&bytes, 16);
    let metadata_offset = read_u64_le(&bytes, 24);
    let metadata_length = read_u64_le(&bytes, 32);
    let leaf_dirs_offset = read_u64_le(&bytes, 40);
    let leaf_dirs_length = read_u64_le(&bytes, 48);
    let data_offset = read_u64_le(&bytes, 56);
    let data_length = read_u64_le(&bytes, 64);

    assert_eq!(root_dir_offset, 127);
    assert!(root_dir_length > 0);
    assert_eq!(metadata_offset, root_dir_offset + root_dir_length);
    assert!(metadata_length > 0);
    assert_eq!(leaf_dirs_offset, metadata_offset + metadata_length);
    assert_eq!(leaf_dirs_length, 0, "single-tile archive should be root-only");
    assert!(data_offset >= leaf_dirs_offset + leaf_dirs_length);

    let file_len = bytes.len() as u64;
    assert!(root_dir_offset + root_dir_length <= file_len);
    assert!(metadata_offset + metadata_length <= file_len);
    assert!(data_offset + data_length <= file_len);
}

#[test]
fn root_leaf_boundary_switches_at_threshold() {
    // finalize_directories() uses MAX_ROOT_ENTRIES=16384.
    const ROOT_THRESHOLD: usize = 16384;
    let make_config = || PmtilesConfig {
        min_zoom: 8,
        max_zoom: 8,
        bounds: (-180.0, -85.0, 180.0, 85.0),
        center: (0.0, 0.0, 8),
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

    let threshold_num_entries = read_u64_le(&at_threshold_bytes, 80);
    let threshold_leaf_dirs_length = read_u64_le(&at_threshold_bytes, 48);
    assert_eq!(threshold_num_entries, ROOT_THRESHOLD as u64);
    assert_eq!(threshold_leaf_dirs_length, 0, "threshold case should remain root-only");

    let over_num_entries = read_u64_le(&over_threshold_bytes, 80);
    let over_leaf_dirs_length = read_u64_le(&over_threshold_bytes, 48);
    assert_eq!(over_num_entries, (ROOT_THRESHOLD + 1) as u64);
    assert!(
        over_leaf_dirs_length > 0,
        "threshold+1 case should use leaf directories"
    );
}
