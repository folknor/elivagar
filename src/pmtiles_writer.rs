// PMTiles v3 archive writer.
//
// Counterpart to the reader in `tile_server.rs`. Writes a clustered,
// gzip-compressed PMTiles archive with Hilbert-ordered tile IDs.

use std::collections::HashMap;
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// PMTiles writer configuration.
pub struct PmtilesConfig {
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// (min_lon, min_lat, max_lon, max_lat)
    pub bounds: (f64, f64, f64, f64),
    /// (lon, lat, zoom)
    pub center: (f64, f64, u8),
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// A directory entry ready for serialization.
struct DirEntry {
    tile_id: u64,
    offset: u64,
    length: u32,
    run_length: u32,
}

/// Stored tile: either unique data or a reference to another tile's data.
enum StoredTile {
    /// Unique tile data at this offset/length in the blob.
    Unique { offset: u64, length: u32 },
    /// Duplicate — points to the same offset/length as the original.
    Dedup { offset: u64, length: u32 },
}

/// Tile data storage: in-memory or streamed to a temp file.
enum TileBlob {
    /// All tile data in a Vec (original behavior, for tests and small runs).
    Memory(Vec<u8>),
    /// Tile data streamed to a temp file on disk.
    File {
        writer: BufWriter<File>,
        path: PathBuf,
        offset: u64,
    },
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Accumulates tiles and writes a PMTiles v3 archive.
pub struct PmtilesWriter {
    config: PmtilesConfig,
    /// Concatenated compressed tile data (in-memory or file-backed).
    blob: TileBlob,
    /// Per-tile metadata in insertion order: (tile_id, stored).
    tiles: Vec<(u64, StoredTile)>,
    /// Content hash -> (offset, length) for dedup.
    dedup: HashMap<u64, (u64, u32)>,
    /// Number of unique tile contents (after dedup).
    unique_count: u64,
}

impl PmtilesWriter {
    /// Create an in-memory writer (tile data kept in a Vec).
    pub fn new(config: PmtilesConfig) -> Self {
        PmtilesWriter {
            config,
            blob: TileBlob::Memory(Vec::new()),
            tiles: Vec::new(),
            dedup: HashMap::new(),
            unique_count: 0,
        }
    }

    /// Create a streaming writer (tile data written to a temp file in `tmp_dir`).
    pub fn new_streaming(config: PmtilesConfig, tmp_dir: &Path) -> io::Result<Self> {
        let blob_path = tmp_dir.join("tiles.blob");
        let file = File::create(&blob_path)?;
        let writer = BufWriter::with_capacity(1 << 20, file); // 1 MB buffer
        Ok(PmtilesWriter {
            config,
            blob: TileBlob::File { writer, path: blob_path, offset: 0 },
            tiles: Vec::new(),
            dedup: HashMap::new(),
            unique_count: 0,
        })
    }

    /// Add a tile. `data` must already be gzip-compressed.
    /// Tiles MUST be added in Hilbert order (tile_id monotonically non-decreasing).
    /// Returns `true` if unique, `false` if deduplicated.
    #[allow(clippy::cast_possible_truncation)]
    pub fn add_tile(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> io::Result<bool> {
        let tile_id = xy_to_tile_id(z, x, y);

        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        let hash = hasher.finish();

        if let Some(&(dup_offset, dup_length)) = self.dedup.get(&hash) {
            self.tiles.push((
                tile_id,
                StoredTile::Dedup {
                    offset: dup_offset,
                    length: dup_length,
                },
            ));
            return Ok(false);
        }

        let offset;
        let length = data.len() as u32;
        match &mut self.blob {
            TileBlob::Memory(vec) => {
                offset = vec.len() as u64;
                vec.extend_from_slice(data);
            }
            TileBlob::File { writer, offset: file_offset, .. } => {
                offset = *file_offset;
                writer.write_all(data)?;
                *file_offset += data.len() as u64;
            }
        }

        self.dedup.insert(hash, (offset, length));
        self.tiles.push((tile_id, StoredTile::Unique { offset, length }));
        self.unique_count += 1;
        Ok(true)
    }

    /// Write the complete PMTiles archive to a file.
    pub fn write_to(&mut self, path: &Path) -> io::Result<()> {
        let entries = self.build_dir_entries();
        let metadata_json = build_metadata(&self.config);

        let (root_bytes, leaf_bytes) = self.build_directories(&entries)?;
        let metadata_compressed = gzip_compress(metadata_json.as_bytes())?;

        // Determine tile data length.
        let data_length = match &self.blob {
            TileBlob::Memory(vec) => vec.len() as u64,
            TileBlob::File { offset, .. } => *offset,
        };

        // Layout: [header 127] [root_dir] [metadata] [leaf_dirs] [tile_data]
        let root_dir_offset: u64 = 127;
        let root_dir_length = root_bytes.len() as u64;
        let metadata_offset = root_dir_offset + root_dir_length;
        let metadata_length = metadata_compressed.len() as u64;
        let leaf_dirs_offset = metadata_offset + metadata_length;
        let leaf_dirs_length = leaf_bytes.len() as u64;
        let data_offset = leaf_dirs_offset + leaf_dirs_length;

        let header = self.build_header(
            root_dir_offset,
            root_dir_length,
            metadata_offset,
            metadata_length,
            leaf_dirs_offset,
            leaf_dirs_length,
            data_offset,
            data_length,
        );

        let file = File::create(path)?;
        let mut w = BufWriter::new(file);
        w.write_all(&header)?;
        w.write_all(&root_bytes)?;
        w.write_all(&metadata_compressed)?;
        w.write_all(&leaf_bytes)?;

        // Write tile data from the appropriate backend.
        match &mut self.blob {
            TileBlob::Memory(vec) => {
                w.write_all(vec)?;
            }
            TileBlob::File { writer, path: blob_path, .. } => {
                writer.flush()?;
                let blob_file = File::open(&*blob_path)?;
                let mut reader = BufReader::with_capacity(1 << 20, blob_file);
                io::copy(&mut reader, &mut w)?;
                drop(reader);
                std::fs::remove_file(&*blob_path)?;
            }
        }

        w.flush()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PmtilesWriter private helpers
// ---------------------------------------------------------------------------

impl PmtilesWriter {
    /// Convert stored tiles into directory entries with run-length encoding.
    fn build_dir_entries(&self) -> Vec<DirEntry> {
        if self.tiles.is_empty() {
            return Vec::new();
        }

        let mut entries: Vec<DirEntry> = Vec::new();

        for &(tile_id, ref stored) in &self.tiles {
            let (offset, length) = match *stored {
                StoredTile::Unique { offset, length } => (offset, length),
                StoredTile::Dedup { offset, length } => (offset, length),
            };
            try_extend_run(&mut entries, tile_id, offset, length);
        }

        entries
    }

    /// Build root and leaf directory bytes. Returns (root_compressed, leaf_compressed).
    fn build_directories(&self, entries: &[DirEntry]) -> io::Result<(Vec<u8>, Vec<u8>)> {
        const MAX_ROOT_ENTRIES: usize = 16384;
        const LEAF_SIZE: usize = 4096;

        if entries.len() <= MAX_ROOT_ENTRIES {
            let root_raw = encode_directory(entries);
            let root_compressed = gzip_compress(&root_raw)?;
            return Ok((root_compressed, Vec::new()));
        }

        build_leaf_directories(entries, LEAF_SIZE)
    }

    /// Build the 127-byte header.
    #[allow(clippy::too_many_arguments)]
    fn build_header(
        &self,
        root_dir_offset: u64,
        root_dir_length: u64,
        metadata_offset: u64,
        metadata_length: u64,
        leaf_dirs_offset: u64,
        leaf_dirs_length: u64,
        data_offset: u64,
        data_length: u64,
    ) -> [u8; 127] {
        let mut h = [0u8; 127];

        h[0..7].copy_from_slice(b"PMTiles");
        h[7] = 3;

        write_u64_le(&mut h, 8, root_dir_offset);
        write_u64_le(&mut h, 16, root_dir_length);
        write_u64_le(&mut h, 24, metadata_offset);
        write_u64_le(&mut h, 32, metadata_length);
        write_u64_le(&mut h, 40, leaf_dirs_offset);
        write_u64_le(&mut h, 48, leaf_dirs_length);
        write_u64_le(&mut h, 56, data_offset);
        write_u64_le(&mut h, 64, data_length);

        write_header_counts(&mut h, &self.tiles, self.unique_count);

        // Clustered
        h[96] = 1;
        // Internal compression: gzip
        h[97] = 2;
        // Tile compression: gzip
        h[98] = 2;
        // Tile type: MVT
        h[99] = 1;

        h[100] = self.config.min_zoom;
        h[101] = self.config.max_zoom;

        write_header_bounds(&mut h, &self.config);

        h
    }
}

/// Write tile count fields into header bytes 72..96.
#[allow(clippy::cast_possible_truncation)]
fn write_header_counts(h: &mut [u8; 127], tiles: &[(u64, StoredTile)], unique_count: u64) {
    let num_addressed = tiles.len() as u64;
    write_u64_le(h, 72, num_addressed);
    write_u64_le(h, 80, num_addressed);
    write_u64_le(h, 88, unique_count);
}

/// Write bounds and center fields into header bytes 102..127.
fn write_header_bounds(h: &mut [u8; 127], config: &PmtilesConfig) {
    let (min_lon, min_lat, max_lon, max_lat) = config.bounds;
    write_i32_le(h, 102, f64_to_e7(min_lon));
    write_i32_le(h, 106, f64_to_e7(min_lat));
    write_i32_le(h, 110, f64_to_e7(max_lon));
    write_i32_le(h, 114, f64_to_e7(max_lat));

    let (center_lon, center_lat, center_zoom) = config.center;
    h[118] = center_zoom;
    write_i32_le(h, 119, f64_to_e7(center_lon));
    write_i32_le(h, 123, f64_to_e7(center_lat));
}

/// Try to extend the last entry's run, or push a new entry.
fn try_extend_run(entries: &mut Vec<DirEntry>, tile_id: u64, offset: u64, length: u32) {
    if let Some(last) = entries.last_mut() {
        let next_id = last.tile_id + u64::from(last.run_length);
        let next_offset = last.offset + u64::from(last.length) * u64::from(last.run_length);
        if tile_id == next_id && offset == next_offset && length == last.length {
            last.run_length += 1;
            return;
        }
    }
    entries.push(DirEntry {
        tile_id,
        offset,
        length,
        run_length: 1,
    });
}

/// Build leaf directories when entries exceed the root limit.
/// Returns (root_compressed, all_leaves_compressed).
fn build_leaf_directories(
    entries: &[DirEntry],
    leaf_size: usize,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut leaf_blob: Vec<u8> = Vec::new();
    let mut root_entries: Vec<DirEntry> = Vec::new();

    for chunk in entries.chunks(leaf_size) {
        let first_tile_id = match chunk.first() {
            Some(e) => e.tile_id,
            None => continue,
        };

        let leaf_raw = encode_directory(chunk);
        let leaf_compressed = gzip_compress(&leaf_raw)?;

        #[allow(clippy::cast_possible_truncation)]
        let leaf_len = leaf_compressed.len() as u32;
        let leaf_offset = leaf_blob.len() as u64;
        leaf_blob.extend_from_slice(&leaf_compressed);

        // run_length=0 marks a leaf directory pointer
        root_entries.push(DirEntry {
            tile_id: first_tile_id,
            offset: leaf_offset,
            length: leaf_len,
            run_length: 0,
        });
    }

    let root_raw = encode_directory(&root_entries);
    let root_compressed = gzip_compress(&root_raw)?;

    Ok((root_compressed, leaf_blob))
}

// ---------------------------------------------------------------------------
// Directory encoding (columnar varint format)
// ---------------------------------------------------------------------------

/// Encode directory entries in PMTiles v3 columnar format.
fn encode_directory(entries: &[DirEntry]) -> Vec<u8> {
    let mut buf = Vec::new();

    #[allow(clippy::cast_possible_truncation)]
    let count = entries.len() as u64;
    encode_varint(&mut buf, count);

    // Column 1: delta-encoded tile IDs
    encode_tile_id_column(&mut buf, entries);

    // Column 2: run lengths
    for e in entries {
        encode_varint(&mut buf, u64::from(e.run_length));
    }

    // Column 3: lengths
    for e in entries {
        encode_varint(&mut buf, u64::from(e.length));
    }

    // Column 4: offsets (0 = contiguous with previous, else offset + 1)
    encode_offset_column(&mut buf, entries);

    buf
}

/// Encode delta-encoded tile ID column.
fn encode_tile_id_column(buf: &mut Vec<u8>, entries: &[DirEntry]) {
    let mut prev_tile_id: u64 = 0;
    for e in entries {
        let delta = e.tile_id - prev_tile_id;
        encode_varint(buf, delta);
        prev_tile_id = e.tile_id;
    }
}

/// Encode offset column with contiguity optimization.
fn encode_offset_column(buf: &mut Vec<u8>, entries: &[DirEntry]) {
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            let prev = &entries[i - 1];
            let expected = prev.offset + u64::from(prev.length);
            if e.offset == expected {
                encode_varint(buf, 0);
            } else {
                encode_varint(buf, e.offset + 1);
            }
        } else {
            encode_varint(buf, e.offset + 1);
        }
    }
}

// ---------------------------------------------------------------------------
// Varint encoding
// ---------------------------------------------------------------------------

/// Encode a u64 as a variable-length integer (LEB128).
fn encode_varint(buf: &mut Vec<u8>, mut val: u64) {
    loop {
        if val < 0x80 {
            #[allow(clippy::cast_possible_truncation)]
            buf.push(val as u8);
            break;
        }
        #[allow(clippy::cast_possible_truncation)]
        buf.push((val as u8 & 0x7F) | 0x80);
        val >>= 7;
    }
}

/// Decode a varint (for tests). Same logic as `tile_server::decode_varint`.
#[cfg(test)]
fn decode_varint(data: &[u8], pos: &mut usize) -> u64 {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
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

// ---------------------------------------------------------------------------
// Gzip compression helper
// ---------------------------------------------------------------------------

fn gzip_compress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

// ---------------------------------------------------------------------------
// Metadata JSON
// ---------------------------------------------------------------------------

fn build_metadata(config: &PmtilesConfig) -> String {
    let layers: &[(&str, u8, u8)] = &[
        ("water_polygons", 4, 14),
        ("water_polygons_labels", 4, 14),
        ("water_lines", 9, 14),
        ("water_lines_labels", 9, 14),
        ("dam_lines", 12, 14),
        ("dam_polygons", 12, 14),
        ("pier_lines", 12, 14),
        ("pier_polygons", 12, 14),
        ("boundaries", 0, 14),
        ("boundary_labels", 2, 14),
        ("place_labels", 4, 14),
        ("land", 7, 14),
        ("sites", 14, 14),
        ("buildings", 14, 14),
        ("addresses", 14, 14),
        ("streets", 5, 14),
        ("street_polygons", 11, 14),
        ("street_labels", 10, 14),
        ("street_labels_points", 12, 14),
        ("streets_polygons_labels", 14, 14),
        ("bridges", 12, 14),
        ("aerialways", 12, 14),
        ("ferries", 10, 14),
        ("public_transport", 11, 14),
        ("pois", 14, 14),
    ];

    let mut layer_arr = String::from("[");
    for (i, &(name, min_z, max_z)) in layers.iter().enumerate() {
        if i > 0 {
            layer_arr.push(',');
        }
        layer_arr.push_str(&format!(
            r#"{{"id":"{name}","minzoom":{min_z},"maxzoom":{max_z}}}"#,
        ));
    }
    layer_arr.push(']');

    format!(
        r#"{{"name":"Shortbread","format":"pbf","type":"baselayer","minzoom":{},"maxzoom":{},"vector_layers":{layer_arr}}}"#,
        config.min_zoom, config.max_zoom,
    )
}

// ---------------------------------------------------------------------------
// Binary helpers
// ---------------------------------------------------------------------------

fn write_u64_le(buf: &mut [u8], offset: usize, val: u64) {
    buf[offset..offset + 8].copy_from_slice(&val.to_le_bytes());
}

fn write_i32_le(buf: &mut [u8], offset: usize, val: i32) {
    buf[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
}

/// Convert a floating-point coordinate to E7 (1e-7 degree units).
#[allow(clippy::cast_possible_truncation)]
fn f64_to_e7(val: f64) -> i32 {
    (val * 1e7) as i32
}

// ---------------------------------------------------------------------------
// Hilbert curve: (z, x, y) <-> tile_id
// ---------------------------------------------------------------------------

/// Convert (z, x, y) to PMTiles Hilbert tile ID.
#[allow(clippy::cast_possible_truncation)]
pub fn xy_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
    if z == 0 {
        return 0;
    }
    let n = 1u64 << z;
    // Cumulative tiles for zoom levels 0..z-1: (4^z - 1) / 3
    let base = (n * n - 1) / 3;
    let d = hilbert_xy2d(n as u32, x, y);
    base + d
}

/// Convert PMTiles Hilbert tile ID back to (z, x, y).
#[allow(clippy::cast_possible_truncation)]
pub fn tile_id_to_zxy(tile_id: u64) -> (u8, u32, u32) {
    if tile_id == 0 {
        return (0, 0, 0);
    }

    // Find zoom level z where base(z) <= tile_id < base(z+1).
    // base(z) = (4^z - 1) / 3
    let mut z: u8 = 0;
    loop {
        z += 1;
        let n = 1u64 << z;
        let next_base = (n * n * 4 - 1) / 3;
        if tile_id < next_base || z >= 31 {
            break;
        }
    }

    let n = 1u64 << z;
    let base = (n * n - 1) / 3;
    let d = tile_id - base;
    let (x, y) = hilbert_d2xy(n as u32, d);
    (z, x, y)
}

#[allow(clippy::cast_possible_truncation)]
fn hilbert_xy2d(n: u32, x: u32, y: u32) -> u64 {
    let mut d: u64 = 0;
    let (mut x, mut y) = (x, y);
    let mut s = n / 2;
    while s > 0 {
        let rx: u32 = u32::from((x & s) > 0);
        let ry: u32 = u32::from((y & s) > 0);
        d += (s as u64 * s as u64) * u64::from((3 * rx) ^ ry);
        hilbert_rot(s, &mut x, &mut y, rx, ry);
        s /= 2;
    }
    d
}

fn hilbert_d2xy(n: u32, d: u64) -> (u32, u32) {
    let mut x: u32 = 0;
    let mut y: u32 = 0;
    let mut d = d;
    let mut s: u32 = 1;
    while s < n {
        // Extract 2-bit quadrant value. Mapping of (3*rx)^ry:
        //   0 -> (rx=0, ry=0)
        //   1 -> (rx=0, ry=1)
        //   2 -> (rx=1, ry=1)
        //   3 -> (rx=1, ry=0)
        #[allow(clippy::cast_possible_truncation)]
        let val = (d & 3) as u32;
        let rx = u32::from(val >= 2);
        let ry = u32::from(val == 1 || val == 2);
        hilbert_rot(s, &mut x, &mut y, rx, ry);
        x += s * rx;
        y += s * ry;
        d >>= 2;
        s <<= 1;
    }
    (x, y)
}

fn hilbert_rot(n: u32, x: &mut u32, y: &mut u32, rx: u32, ry: u32) {
    if ry == 0 {
        if rx == 1 {
            *x = n - 1 - *x;
            *y = n - 1 - *y;
        }
        std::mem::swap(x, y);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
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
        let test_values: &[u64] = &[0, 1, 127, 128, 255, 256, 300, 16383, 16384, u64::MAX];
        for &val in test_values {
            let mut buf = Vec::new();
            encode_varint(&mut buf, val);
            let mut pos = 0;
            let decoded = decode_varint(&buf, &mut pos);
            assert_eq!(val, decoded, "varint roundtrip failed for {val}");
            assert_eq!(pos, buf.len(), "varint did not consume all bytes for {val}");
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
        let mut pos = 0;

        let count = decode_varint(&encoded, &mut pos);
        assert_eq!(count, 2);

        // Tile IDs (delta): first=5, delta=5
        let id0 = decode_varint(&encoded, &mut pos);
        let id1 = decode_varint(&encoded, &mut pos);
        assert_eq!(id0, 5);
        assert_eq!(id1, 5);

        // Run lengths: 3, 1
        let r0 = decode_varint(&encoded, &mut pos);
        let r1 = decode_varint(&encoded, &mut pos);
        assert_eq!(r0, 3);
        assert_eq!(r1, 1);

        // Lengths: 100, 50
        let l0 = decode_varint(&encoded, &mut pos);
        let l1 = decode_varint(&encoded, &mut pos);
        assert_eq!(l0, 100);
        assert_eq!(l1, 50);

        // Offsets: first=0+1=1, second=0 (contiguous: 0+100=100)
        let o0 = decode_varint(&encoded, &mut pos);
        let o1 = decode_varint(&encoded, &mut pos);
        assert_eq!(o0, 1);
        assert_eq!(o1, 0);
    }

    #[test]
    fn test_directory_non_contiguous_offset() {
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
        let mut pos = 0;

        // Skip count, tile_ids, run_lengths, lengths
        let _count = decode_varint(&encoded, &mut pos);
        let _id0 = decode_varint(&encoded, &mut pos);
        let _id1 = decode_varint(&encoded, &mut pos);
        let _r0 = decode_varint(&encoded, &mut pos);
        let _r1 = decode_varint(&encoded, &mut pos);
        let _l0 = decode_varint(&encoded, &mut pos);
        let _l1 = decode_varint(&encoded, &mut pos);

        let o0 = decode_varint(&encoded, &mut pos);
        let o1 = decode_varint(&encoded, &mut pos);
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
        assert_eq!(layers.len(), 25);
        assert_eq!(layers[0]["id"], "water_polygons");
        assert_eq!(layers[15]["id"], "streets");
    }

    // -----------------------------------------------------------------------
    // Run-length encoding
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_length_encoding() {
        let config = PmtilesConfig {
            min_zoom: 1,
            max_zoom: 1,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            center: (0.0, 0.0, 1),
        };

        let mut writer = PmtilesWriter::new(config);

        // All 4 tiles at z=1 have consecutive Hilbert IDs (1,2,3,4) and
        // gzip of single bytes produces identical compressed sizes, so they
        // merge into a single run.
        let data = gzip_compress(b"x").unwrap();
        let data2 = gzip_compress(b"y").unwrap();
        let data3 = gzip_compress(b"z").unwrap();
        let data4 = gzip_compress(b"w").unwrap();

        writer.add_tile(1, 0, 0, &data).unwrap();
        writer.add_tile(1, 0, 1, &data2).unwrap();
        writer.add_tile(1, 1, 1, &data3).unwrap();
        writer.add_tile(1, 1, 0, &data4).unwrap();

        let entries = writer.build_dir_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].run_length, 4);
    }

    #[test]
    fn test_run_length_same_size() {
        // Directly craft StoredTile entries to test run merging.
        let config = PmtilesConfig {
            min_zoom: 1,
            max_zoom: 1,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            center: (0.0, 0.0, 1),
        };

        let mut writer = PmtilesWriter::new(config);
        writer.tiles.push((1, StoredTile::Unique { offset: 0, length: 100 }));
        writer.tiles.push((2, StoredTile::Unique { offset: 100, length: 100 }));
        writer.tiles.push((3, StoredTile::Unique { offset: 200, length: 100 }));
        writer.tiles.push((5, StoredTile::Unique { offset: 300, length: 100 }));

        let entries = writer.build_dir_entries();
        // tile_ids 1,2,3 merge into run of 3; tile_id 5 is separate
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].tile_id, 1);
        assert_eq!(entries[0].run_length, 3);
        assert_eq!(entries[1].tile_id, 5);
        assert_eq!(entries[1].run_length, 1);
    }
}
