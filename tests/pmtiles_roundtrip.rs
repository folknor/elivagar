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
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use elivagar::pmtiles_writer::{tile_id_to_zxy, xy_to_tile_id, PmtilesConfig, PmtilesWriter};
use libdeflater::{CompressionLvl, Compressor, Decompressor};

// ---------------------------------------------------------------------------
// PMTiles reader (minimal, sync) — adapted from examples/compare_tiles.rs
// ---------------------------------------------------------------------------

struct PmtilesReader {
    file: File,
    header: [u8; 127],
    root_dir_offset: u64,
    root_dir_length: u64,
    leaf_dirs_offset: u64,
    data_offset: u64,
    internal_compression: u8,
}

struct TileEntry {
    tile_id: u64,
    offset: u64,
    length: u32,
}

struct RawDirEntry {
    tile_id: u64,
    offset: u64,
    length: u32,
    run_length: u32,
}

impl PmtilesReader {
    fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut header = [0u8; 127];
        file.read_exact(&mut header)?;

        if &header[0..7] != b"PMTiles" || header[7] != 3 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not PMTiles v3"));
        }

        Ok(PmtilesReader {
            file,
            header,
            root_dir_offset: read_u64_le(&header, 8),
            root_dir_length: read_u64_le(&header, 16),
            leaf_dirs_offset: read_u64_le(&header, 40),
            data_offset: read_u64_le(&header, 56),
            internal_compression: header[97],
        })
    }

    fn min_zoom(&self) -> u8 {
        self.header[100]
    }

    fn max_zoom(&self) -> u8 {
        self.header[101]
    }

    fn tile_type(&self) -> u8 {
        self.header[99]
    }

    fn num_addressed(&self) -> u64 {
        read_u64_le(&self.header, 72)
    }

    fn num_entries(&self) -> u64 {
        read_u64_le(&self.header, 80)
    }

    fn num_unique(&self) -> u64 {
        read_u64_le(&self.header, 88)
    }

    fn metadata_offset(&self) -> u64 {
        read_u64_le(&self.header, 24)
    }

    fn metadata_length(&self) -> u64 {
        read_u64_le(&self.header, 32)
    }

    fn read_metadata(&mut self) -> io::Result<String> {
        let offset = self.metadata_offset();
        let length = self.metadata_length();
        self.file.seek(SeekFrom::Start(offset))?;
        let mut compressed = vec![0u8; length as usize];
        self.file.read_exact(&mut compressed)?;

        if self.internal_compression == 2 {
            let buf = gzip_decompress(&compressed)?;
            Ok(String::from_utf8_lossy(&buf).to_string())
        } else {
            Ok(String::from_utf8_lossy(&compressed).to_string())
        }
    }

    fn read_all_entries(&mut self) -> io::Result<Vec<TileEntry>> {
        let root_entries = self.read_directory(self.root_dir_offset, self.root_dir_length)?;
        let mut all_entries = Vec::new();

        for entry in &root_entries {
            if entry.run_length == 0 {
                let leaf_offset = self.leaf_dirs_offset + entry.offset;
                let leaf_entries = self.read_directory(leaf_offset, entry.length as u64)?;
                expand_entries(&leaf_entries, &mut all_entries);
            } else {
                expand_single(entry, &mut all_entries);
            }
        }

        Ok(all_entries)
    }

    fn read_directory(&mut self, offset: u64, length: u64) -> io::Result<Vec<RawDirEntry>> {
        self.file.seek(SeekFrom::Start(offset))?;
        let mut compressed = vec![0u8; length as usize];
        self.file.read_exact(&mut compressed)?;

        let raw = if self.internal_compression == 2 {
            gzip_decompress(&compressed)?
        } else {
            compressed
        };

        Ok(decode_directory(&raw))
    }

    fn read_tile(&mut self, entry: &TileEntry) -> io::Result<Vec<u8>> {
        let abs_offset = self.data_offset + entry.offset;
        self.file.seek(SeekFrom::Start(abs_offset))?;
        let mut compressed = vec![0u8; entry.length as usize];
        self.file.read_exact(&mut compressed)?;
        gzip_decompress(&compressed)
    }
}

fn expand_entries(dir_entries: &[RawDirEntry], out: &mut Vec<TileEntry>) {
    for e in dir_entries {
        if e.run_length == 0 {
            continue;
        }
        expand_single(e, out);
    }
}

fn expand_single(e: &RawDirEntry, out: &mut Vec<TileEntry>) {
    for r in 0..e.run_length {
        out.push(TileEntry {
            tile_id: e.tile_id + u64::from(r),
            offset: e.offset,
            length: e.length,
        });
    }
}

fn decode_directory(data: &[u8]) -> Vec<RawDirEntry> {
    let mut pos = 0;
    let count = decode_varint(data, &mut pos) as usize;

    let mut tile_ids = Vec::with_capacity(count);
    let mut prev: u64 = 0;
    for _ in 0..count {
        let delta = decode_varint(data, &mut pos);
        prev += delta;
        tile_ids.push(prev);
    }

    let mut run_lengths = Vec::with_capacity(count);
    for _ in 0..count {
        run_lengths.push(decode_varint(data, &mut pos) as u32);
    }

    let mut lengths = Vec::with_capacity(count);
    for _ in 0..count {
        lengths.push(decode_varint(data, &mut pos) as u32);
    }

    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let v = decode_varint(data, &mut pos);
        let offset = if v == 0 && i > 0 {
            let prev: &RawDirEntry = &entries[i - 1];
            // Contiguous: prev.offset + prev.length (NOT multiplied by run_length,
            // since all tiles in a run share the same data blob).
            prev.offset + u64::from(prev.length)
        } else {
            v - 1
        };
        entries.push(RawDirEntry {
            tile_id: tile_ids[i],
            offset,
            length: lengths[i],
            run_length: run_lengths[i],
        });
    }

    entries
}

fn decode_varint(data: &[u8], pos: &mut usize) -> u64 {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= data.len() {
            return result;
        }
        let byte = data[*pos];
        *pos += 1;
        result |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    result
}

fn read_u64_le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// MVT protobuf decoder (minimal) — adapted from examples/compare_tiles.rs
// ---------------------------------------------------------------------------

struct MvtLayer {
    name: String,
    feature_count: usize,
}

fn decode_mvt_layers(data: &[u8]) -> Vec<MvtLayer> {
    let mut layers = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        let (field, wire_type, new_pos) = decode_proto_tag(data, pos);
        pos = new_pos;

        if wire_type == 2 {
            let (len, new_pos) = decode_proto_varint(data, pos);
            pos = new_pos;
            let end = pos + len as usize;
            if end > data.len() {
                break;
            }
            if field == 3 {
                layers.push(decode_mvt_layer(&data[pos..end]));
            }
            pos = end;
        } else if wire_type == 0 {
            let (_, new_pos) = decode_proto_varint(data, pos);
            pos = new_pos;
        } else if wire_type == 1 {
            pos += 8;
        } else if wire_type == 5 {
            pos += 4;
        } else {
            break;
        }
    }

    layers
}

fn decode_mvt_layer(data: &[u8]) -> MvtLayer {
    let mut layer = MvtLayer {
        name: String::new(),
        feature_count: 0,
    };

    let mut pos = 0;
    while pos < data.len() {
        let (field, wire_type, new_pos) = decode_proto_tag(data, pos);
        pos = new_pos;

        if wire_type == 2 {
            let (len, new_pos) = decode_proto_varint(data, pos);
            pos = new_pos;
            let end = pos + len as usize;
            if end > data.len() {
                break;
            }
            match field {
                1 => layer.name = String::from_utf8_lossy(&data[pos..end]).to_string(),
                2 => layer.feature_count += 1,
                _ => {}
            }
            pos = end;
        } else if wire_type == 0 {
            let (_, new_pos) = decode_proto_varint(data, pos);
            pos = new_pos;
        } else if wire_type == 1 {
            pos += 8;
        } else if wire_type == 5 {
            pos += 4;
        } else {
            break;
        }
    }

    layer
}

fn decode_proto_tag(data: &[u8], pos: usize) -> (u32, u8, usize) {
    let (val, new_pos) = decode_proto_varint(data, pos);
    let field = (val >> 3) as u32;
    let wire_type = (val & 7) as u8;
    (field, wire_type, new_pos)
}

fn decode_proto_varint(data: &[u8], mut pos: usize) -> (u64, usize) {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        if pos >= data.len() {
            return (result, pos);
        }
        let byte = data[pos];
        pos += 1;
        result |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (result, pos)
}

// ---------------------------------------------------------------------------
// MVT protobuf encoder (minimal — builds tiles with named layers)
// ---------------------------------------------------------------------------

/// Encode a minimal valid MVT tile with the given layers.
/// Each layer gets one point feature at (0,0).
fn encode_mvt_tile(layer_names: &[&str]) -> Vec<u8> {
    let mut tile = Vec::new();
    for name in layer_names {
        let layer_bytes = encode_mvt_layer(name);
        // field 3 (Tile.layers), wire type 2 (length-delimited)
        encode_proto_field(&mut tile, 3, &layer_bytes);
    }
    tile
}

fn encode_mvt_layer(name: &str) -> Vec<u8> {
    let mut layer = Vec::new();

    // field 1: name (string)
    encode_proto_field(&mut layer, 1, name.as_bytes());

    // field 2: feature (one point at 0,0)
    let feature = encode_mvt_point_feature();
    encode_proto_field(&mut layer, 2, &feature);

    // field 5: extent (varint) = 4096
    encode_proto_varint_field(&mut layer, 5, 4096);

    // field 15: version (varint) = 2
    encode_proto_varint_field(&mut layer, 15, 2);

    layer
}

fn encode_mvt_point_feature() -> Vec<u8> {
    let mut feature = Vec::new();

    // field 3: type = POINT (1)
    encode_proto_varint_field(&mut feature, 3, 1);

    // field 4: geometry (packed uint32) — MoveTo(1, dx=0, dy=0)
    // MoveTo command: (1 << 3) | 1 = 9
    // param 0 (zigzag): 0
    // param 1 (zigzag): 0
    let mut geom = Vec::new();
    encode_proto_varint_raw(&mut geom, 9); // MoveTo, count=1
    encode_proto_varint_raw(&mut geom, 0); // dx=0
    encode_proto_varint_raw(&mut geom, 0); // dy=0
    encode_proto_field(&mut feature, 4, &geom);

    feature
}

fn encode_proto_field(buf: &mut Vec<u8>, field_number: u32, data: &[u8]) {
    let tag = (u64::from(field_number) << 3) | 2; // wire type 2 = length-delimited
    encode_proto_varint_raw(buf, tag);
    encode_proto_varint_raw(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

fn encode_proto_varint_field(buf: &mut Vec<u8>, field_number: u32, value: u64) {
    let tag = (u64::from(field_number) << 3) | 0; // wire type 0 = varint
    encode_proto_varint_raw(buf, tag);
    encode_proto_varint_raw(buf, value);
}

fn encode_proto_varint_raw(buf: &mut Vec<u8>, mut val: u64) {
    loop {
        if val < 0x80 {
            buf.push(val as u8);
            break;
        }
        buf.push((val as u8 & 0x7F) | 0x80);
        val >>= 7;
    }
}

// ---------------------------------------------------------------------------
// Gzip helper
// ---------------------------------------------------------------------------

fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    let mut compressor = Compressor::new(CompressionLvl::default());
    let bound = compressor.gzip_compress_bound(data.len());
    let mut out = vec![0u8; bound];
    let n = compressor.gzip_compress(data, &mut out).unwrap();
    out.truncate(n);
    out
}

fn gzip_decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut decompressor = Decompressor::new();
    let mut buf = vec![0u8; data.len() * 8];
    loop {
        match decompressor.gzip_decompress(data, &mut buf) {
            Ok(n) => {
                buf.truncate(n);
                return Ok(buf);
            }
            Err(libdeflater::DecompressionError::InsufficientSpace) => {
                buf.resize(buf.len() * 2, 0);
            }
            Err(e) => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")));
            }
        }
    }
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
    assert_eq!(&reader.header[0..7], b"PMTiles");
    assert_eq!(reader.header[7], 3);

    // Zoom range
    assert_eq!(reader.min_zoom(), 2);
    assert_eq!(reader.max_zoom(), 10);

    // Tile type = MVT (1)
    assert_eq!(reader.tile_type(), 1);

    // Clustered = 1
    assert_eq!(reader.header[96], 1);

    // Internal compression = gzip (2)
    assert_eq!(reader.header[97], 2);

    // Tile compression = gzip (2)
    assert_eq!(reader.header[98], 2);

    // Tile counts
    assert_eq!(reader.num_addressed(), 1);
    assert_eq!(reader.num_unique(), 1);

    // Bounds (E7 encoding): 8.0 -> 80000000, 54.5 -> 545000000, etc.
    let min_lon = i32::from_le_bytes(reader.header[102..106].try_into().unwrap());
    let min_lat = i32::from_le_bytes(reader.header[106..110].try_into().unwrap());
    let max_lon = i32::from_le_bytes(reader.header[110..114].try_into().unwrap());
    let max_lat = i32::from_le_bytes(reader.header[114..118].try_into().unwrap());
    assert_eq!(min_lon, 80_000_000);
    assert_eq!(min_lat, 545_000_000);
    assert_eq!(max_lon, 152_000_000);
    assert_eq!(max_lat, 578_000_000);

    // Center
    let center_zoom = reader.header[118];
    let center_lon = i32::from_le_bytes(reader.header[119..123].try_into().unwrap());
    let center_lat = i32::from_le_bytes(reader.header[123..127].try_into().unwrap());
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
        let layers = decode_mvt_layers(&raw);

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
        let layers = decode_mvt_layers(&raw);
        assert!(!layers.is_empty(), "tile should have at least one layer");
    }

    // Verify the unique tile has the right layers.
    // z1/1/1 is tile_id = xy_to_tile_id(1,1,1) = 3
    let unique_id = xy_to_tile_id(1, 1, 1);
    let unique_entry = entries.iter().find(|e| e.tile_id == unique_id).unwrap();
    let layers = decode_mvt_layers(&reader.read_tile(unique_entry).unwrap());
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
        threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
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
        let layers = decode_mvt_layers(&raw);

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
