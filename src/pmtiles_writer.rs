//! PMTiles v3 archive writer.
//!
//! Writes clustered, gzip-compressed PMTiles archives with Hilbert-ordered
//! tile IDs and content deduplication. Supports both in-memory and streaming
//! modes for the tile blob and directory entries.
//!
//! # Example
//!
//! ```no_run
//! use elivagar::pmtiles_writer::{PmtilesConfig, PmtilesWriter};
//!
//! let config = PmtilesConfig {
//!     min_zoom: 0,
//!     max_zoom: 14,
//!     bounds: (8.0, 54.5, 15.2, 57.8),
//!     center: (11.5, 56.0, 7),
//! };
//! let mut writer = PmtilesWriter::new(config);
//!
//! // Tiles must be added in Hilbert order.
//! let gzipped_mvt = vec![0u8; 100]; // pre-compressed MVT data
//! writer.add_tile(0, 0, 0, &gzipped_mvt).expect("add tile");
//!
//! writer.write_to(std::path::Path::new("output.pmtiles")).expect("write");
//! ```

use std::collections::HashMap;
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufReader, BufWriter, Read as _, Write};
use std::path::{Path, PathBuf};

use libdeflater::{CompressionLvl, Compressor};
use protohoggr::encode_varint;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// PMTiles writer configuration.
pub struct PmtilesConfig {
    /// Minimum zoom level in the archive.
    pub min_zoom: u8,
    /// Maximum zoom level in the archive.
    pub max_zoom: u8,
    /// Geographic bounds as (min_lon, min_lat, max_lon, max_lat) in WGS84.
    pub bounds: (f64, f64, f64, f64),
    /// Default map center as (lon, lat, zoom) in WGS84.
    pub center: (f64, f64, u8),
}

/// Maximum number of entries in the dedup HashMap before we stop inserting.
/// At planet scale, unlimited dedup grows to ~7 GB. Capping at 1M entries
/// keeps the map under ~50 MB while still deduplicating the ocean fill tiles
/// (which are added early and remain cached).
const MAX_DEDUP_ENTRIES: usize = 1_000_000;

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
const _: () = assert!(std::mem::size_of::<DirEntry>() == 24);

/// Storage for directory entries: in-memory or streamed to a temp file.
/// Streaming avoids accumulating all ~200M+ directory entries in RAM at
/// planet scale. Entries are built incrementally with run-length encoding
/// in push_dir_entry(), so the on-disk format is already compacted.
enum DirStore {
    Memory(Vec<DirEntry>),
    Streaming {
        writer: BufWriter<File>,
        path: PathBuf,
        count: u64,
    },
}

/// Tile data storage: in-memory or streamed to a temp file.
/// Streaming mode avoids buffering all compressed tile data in RAM (~3 GB
/// for a planet). The blob is written sequentially and read back during
/// write_to() via io::copy.
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
///
/// Tiles must be added in Hilbert order via [`add_tile()`](Self::add_tile).
/// Duplicate tile contents are automatically deduplicated using SipHash.
///
/// Two storage modes are available:
/// - [`new()`](Self::new) — in-memory (tile data in a `Vec`). Best for small
///   extracts and tests.
/// - [`new_streaming()`](Self::new_streaming) — file-backed (tile data and
///   directory entries streamed to disk). Required for planet-scale runs to
///   avoid multi-GB RAM usage.
pub struct PmtilesWriter {
    config: PmtilesConfig,
    /// Concatenated compressed tile data (in-memory or file-backed).
    blob: TileBlob,
    /// Total number of tiles addressed (including deduped references).
    num_addressed: u64,
    /// Current run being built (flushed when a new non-extending tile arrives).
    current_run: Option<DirEntry>,
    /// Directory entries (in-memory or streamed to disk).
    dir_store: DirStore,
    /// Content hash -> (offset, length) for dedup.
    dedup: HashMap<u64, (u64, u32)>,
    /// Number of unique tile contents (after dedup).
    unique_count: u64,
}

impl PmtilesWriter {
    /// Total tiles added (including deduped references).
    pub fn tile_count(&self) -> u64 {
        self.num_addressed
    }

    /// Unique tile data blobs (after dedup).
    pub fn unique_tile_count(&self) -> u64 {
        self.unique_count
    }
}

impl PmtilesWriter {
    /// Create an in-memory writer (tile data kept in a Vec).
    pub fn new(config: PmtilesConfig) -> Self {
        PmtilesWriter {
            config,
            blob: TileBlob::Memory(Vec::new()),
            num_addressed: 0,
            current_run: None,
            dir_store: DirStore::Memory(Vec::new()),
            dedup: HashMap::new(),
            unique_count: 0,
        }
    }

    /// Create a streaming writer (tile data written to a temp file in `tmp_dir`).
    ///
    /// # Errors
    /// Returns `io::Error` if temp file creation fails.
    pub fn new_streaming(config: PmtilesConfig, tmp_dir: &Path) -> io::Result<Self> {
        let blob_path = tmp_dir.join("tiles.blob");
        let file = File::create(&blob_path)?;
        let writer = BufWriter::with_capacity(1 << 20, file); // 1 MB buffer

        let dir_path = tmp_dir.join("dir_entries.bin");
        let dir_file = File::create(&dir_path)?;
        let dir_writer = BufWriter::with_capacity(1 << 16, dir_file);

        Ok(PmtilesWriter {
            config,
            blob: TileBlob::File { writer, path: blob_path, offset: 0 },
            num_addressed: 0,
            current_run: None,
            dir_store: DirStore::Streaming { writer: dir_writer, path: dir_path, count: 0 },
            dedup: HashMap::new(),
            unique_count: 0,
        })
    }

    /// Add a tile. `data` must already be gzip-compressed.
    /// Tiles MUST be added in Hilbert order (tile_id monotonically non-decreasing).
    /// Returns `true` if unique, `false` if deduplicated.
    ///
    /// # Errors
    /// Returns `io::Error` if writing to the tile blob file fails (streaming mode).
    #[allow(clippy::cast_possible_truncation)]
    #[hotpath::measure]
    pub fn add_tile(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> io::Result<bool> {
        let tile_id = xy_to_tile_id(z, x, y);

        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        let hash = hasher.finish();

        if let Some(&(dup_offset, dup_length)) = self.dedup.get(&hash) {
            // Guard against hash collisions by also checking compressed length.
            // A false dedup requires both a SipHash-1-3 collision (~2^-64) AND
            // matching length (~2^-17), giving ~2^-81 per pair — negligible at
            // planet scale. Full content comparison would require storing tile
            // data or seeking back in the blob file.
            if dup_length == data.len() as u32 {
                self.push_dir_entry(tile_id, dup_offset, dup_length)?;
                return Ok(false);
            }
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

        if self.dedup.len() < MAX_DEDUP_ENTRIES {
            self.dedup.insert(hash, (offset, length));
        }
        self.push_dir_entry(tile_id, offset, length)?;
        self.unique_count += 1;
        Ok(true)
    }

    /// Write the complete PMTiles archive to a file.
    ///
    /// # Errors
    /// Returns `io::Error` if file creation, directory encoding, or data copy fails.
    #[hotpath::measure]
    pub fn write_to(&mut self, path: &Path) -> io::Result<()> {
        // Free dedup map — no longer needed after all tiles are added.
        drop(std::mem::take(&mut self.dedup));

        let entries = self.collect_dir_entries()?;
        let metadata_json = build_metadata(&self.config);

        let (root_bytes, leaf_bytes) = self.build_directories(&entries)?;
        let metadata_compressed = gzip_compress(metadata_json.as_bytes())?;

        // Clean up streaming dir_entries temp file if it exists.
        // Best-effort cleanup of streaming temp file — failure is harmless.
        if let DirStore::Streaming { path: dir_path, .. } = &self.dir_store {
            drop(std::fs::remove_file(dir_path));
        }

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

        // Save count before dropping entries to free ~120 MB at planet scale.
        let num_entries = entries.len() as u64;
        drop(entries);
        let header = self.build_header(
            root_dir_offset,
            root_dir_length,
            metadata_offset,
            metadata_length,
            leaf_dirs_offset,
            leaf_dirs_length,
            data_offset,
            data_length,
            num_entries,
        );

        let file = File::create(path)?;
        let mut w = BufWriter::with_capacity(1 << 20, file);
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
    /// Flush the current run-length entry to the dir store.
    fn flush_run(&mut self) -> io::Result<()> {
        if let Some(run) = self.current_run.take() {
            match &mut self.dir_store {
                DirStore::Memory(entries) => entries.push(run),
                DirStore::Streaming { writer, count, .. } => {
                    writer.write_all(&run.tile_id.to_le_bytes())?;
                    writer.write_all(&run.offset.to_le_bytes())?;
                    writer.write_all(&run.length.to_le_bytes())?;
                    writer.write_all(&run.run_length.to_le_bytes())?;
                    *count += 1;
                }
            }
        }
        Ok(())
    }

    /// Record a directory entry, extending the current run if possible.
    fn push_dir_entry(&mut self, tile_id: u64, offset: u64, length: u32) -> io::Result<()> {
        self.num_addressed += 1;
        if let Some(ref run) = self.current_run {
            let next_id = run.tile_id + u64::from(run.run_length);
            if tile_id == next_id && offset == run.offset && length == run.length {
                self.current_run.as_mut().expect("just checked").run_length += 1;
                return Ok(());
            }
        }
        // Flush old run if any, then start new one
        self.flush_run()?;
        self.current_run = Some(DirEntry { tile_id, offset, length, run_length: 1 });
        Ok(())
    }

    /// Collect all directory entries (flushing the current run and reading back
    /// from the streaming temp file if necessary).
    #[hotpath::measure]
    fn collect_dir_entries(&mut self) -> io::Result<Vec<DirEntry>> {
        self.flush_run()?;
        match &mut self.dir_store {
            DirStore::Memory(entries) => Ok(std::mem::take(entries)),
            DirStore::Streaming { writer, path, count } => {
                writer.flush()?;
                #[allow(clippy::cast_possible_truncation)]
                let num = *count as usize;
                let mut data = Vec::new();
                let mut file = File::open(path)?;
                file.read_to_end(&mut data)?;
                let mut entries = Vec::with_capacity(num);
                let mut pos = 0;
                for _ in 0..num {
                    let tile_id = read_u64_le(&data, &mut pos);
                    let offset = read_u64_le(&data, &mut pos);
                    let length = read_u32_le(&data, &mut pos);
                    let run_length = read_u32_le(&data, &mut pos);
                    entries.push(DirEntry { tile_id, offset, length, run_length });
                }
                Ok(entries)
            }
        }
    }

    /// Build root and leaf directory bytes. Returns (root_compressed, leaf_compressed).
    #[hotpath::measure]
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
        num_entries: u64,
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

        write_header_counts(&mut h, self.num_addressed, num_entries, self.unique_count);

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
fn write_header_counts(h: &mut [u8; 127], num_addressed: u64, num_entries: u64, unique_count: u64) {
    write_u64_le(h, 72, num_addressed);
    write_u64_le(h, 80, num_entries);
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

/// Build leaf directories when entries exceed the root limit.
/// Returns (root_compressed, all_leaves_compressed).
#[hotpath::measure]
fn build_leaf_directories(
    entries: &[DirEntry],
    leaf_size: usize,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut leaf_blob: Vec<u8> = Vec::new();
    let mut root_entries: Vec<DirEntry> = Vec::new();

    // Reuse compressor + output buffer across all leaf chunks.
    let lvl = CompressionLvl::default();
    let mut compressor = Compressor::new(lvl);
    let mut gz_buf: Vec<u8> = Vec::new();

    for chunk in entries.chunks(leaf_size) {
        let first_tile_id = match chunk.first() {
            Some(e) => e.tile_id,
            None => continue,
        };

        let leaf_raw = encode_directory(chunk);
        let bound = compressor.gzip_compress_bound(leaf_raw.len());
        gz_buf.resize(bound, 0);
        let n = compressor.gzip_compress(&leaf_raw, &mut gz_buf)
            .map_err(|e| io::Error::other(format!("{e:?}")))?;

        #[allow(clippy::cast_possible_truncation)]
        let leaf_len = n as u32;
        let leaf_offset = leaf_blob.len() as u64;
        leaf_blob.extend_from_slice(&gz_buf[..n]);

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
#[hotpath::measure]
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
// Gzip compression helper
// ---------------------------------------------------------------------------

fn gzip_compress(data: &[u8]) -> io::Result<Vec<u8>> {
    let lvl = CompressionLvl::default();
    let mut compressor = Compressor::new(lvl);
    let bound = compressor.gzip_compress_bound(data.len());
    let mut out = vec![0u8; bound];
    let n = compressor.gzip_compress(data, &mut out)
        .map_err(|e| io::Error::other(format!("{e:?}")))?;
    out.truncate(n);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Metadata JSON
// ---------------------------------------------------------------------------

/// Build PMTiles metadata JSON. Hand-rolled rather than serde_json to avoid
/// a runtime dependency for ~20 lines of fixed-schema formatting.
/// Validated by test_metadata_json which round-trips through serde_json.
fn build_metadata(config: &PmtilesConfig) -> String {
    use crate::shortbread::Layer;

    let mut layer_arr = String::from("[");
    for (i, &layer) in Layer::ALL.iter().enumerate() {
        if i > 0 {
            layer_arr.push(',');
        }
        let name = layer.name();
        let min_z = layer.min_zoom();
        let max_z = config.max_zoom;
        // format! is fine here — 26-iteration loop, called once per run. Cold path.
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

/// Read a u64 from a byte buffer at the given position, advancing the position.
fn read_u64_le(data: &[u8], pos: &mut usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[*pos..*pos + 8]);
    *pos += 8;
    u64::from_le_bytes(bytes)
}

/// Read a u32 from a byte buffer at the given position, advancing the position.
fn read_u32_le(data: &[u8], pos: &mut usize) -> u32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&data[*pos..*pos + 4]);
    *pos += 4;
    u32::from_le_bytes(bytes)
}

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
#[inline]
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
    // Note: n * n * 4 would overflow u64 at z=31, but the z >= 31 guard
    // fires first in release mode (wrapping). In debug mode the overflow
    // would panic before the guard. Not reachable: max_zoom is validated
    // to 14 at pipeline entry, so tile_ids never iterate past z=14.
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
// Tests (see pmtiles_writer_tests.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pmtiles_writer_tests.rs"]
mod tests;
