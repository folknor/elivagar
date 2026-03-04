//! Minimal PMTiles v3 reader.
//!
//! Provides enough functionality to open a PMTiles archive, traverse its
//! directory entries, read individual tiles, and decode MVT layer structure.
//! Used by the `verify` subcommand and integration tests.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use flate2::read::GzDecoder;
use protohoggr::{Cursor, WIRE_LEN};

// ---------------------------------------------------------------------------
// Binary helpers
// ---------------------------------------------------------------------------

pub fn read_u64_le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

pub fn read_i32_le(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

// ---------------------------------------------------------------------------
// Gzip helper
// ---------------------------------------------------------------------------

pub fn gzip_decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// PMTiles reader
// ---------------------------------------------------------------------------

/// Header size for PMTiles v3.
pub const HEADER_SIZE: usize = 127;

pub struct PmtilesReader {
    file: File,
    header: [u8; HEADER_SIZE],
    root_dir_offset: u64,
    root_dir_length: u64,
    leaf_dirs_offset: u64,
    data_offset: u64,
    internal_compression: u8,
}

pub struct TileEntry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
}

pub struct RawDirEntry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

impl PmtilesReader {
    /// Open a PMTiles file and validate the header magic/version.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut header = [0u8; HEADER_SIZE];
        file.read_exact(&mut header)?;

        if &header[0..7] != b"PMTiles" || header[7] != 3 {
            return Err(io::Error::other("not PMTiles v3"));
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

    pub fn header(&self) -> &[u8; HEADER_SIZE] {
        &self.header
    }

    pub fn file_size(&self) -> io::Result<u64> {
        self.file.metadata().map(|m| m.len())
    }

    pub fn min_zoom(&self) -> u8 {
        self.header[100]
    }

    pub fn max_zoom(&self) -> u8 {
        self.header[101]
    }

    /// Tile type byte: 1=MVT, 2=PNG, 3=JPEG, 4=WebP, 5=AVIF.
    pub fn tile_type(&self) -> u8 {
        self.header[99]
    }

    /// Tile compression byte: 0=unknown, 1=none, 2=gzip, 3=brotli, 4=zstd.
    pub fn tile_compression(&self) -> u8 {
        self.header[98]
    }

    /// Internal compression byte (for directories/metadata).
    pub fn internal_compression(&self) -> u8 {
        self.internal_compression
    }

    pub fn num_addressed(&self) -> u64 {
        read_u64_le(&self.header, 72)
    }

    pub fn num_entries(&self) -> u64 {
        read_u64_le(&self.header, 80)
    }

    pub fn num_unique(&self) -> u64 {
        read_u64_le(&self.header, 88)
    }

    pub fn metadata_offset(&self) -> u64 {
        read_u64_le(&self.header, 24)
    }

    pub fn metadata_length(&self) -> u64 {
        read_u64_le(&self.header, 32)
    }

    pub fn root_dir_offset(&self) -> u64 {
        self.root_dir_offset
    }

    pub fn root_dir_length(&self) -> u64 {
        self.root_dir_length
    }

    pub fn leaf_dirs_offset(&self) -> u64 {
        self.leaf_dirs_offset
    }

    pub fn leaf_dirs_length(&self) -> u64 {
        read_u64_le(&self.header, 48)
    }

    pub fn data_offset(&self) -> u64 {
        self.data_offset
    }

    pub fn data_length(&self) -> u64 {
        read_u64_le(&self.header, 64)
    }

    /// Read and decompress the metadata section as a UTF-8 string.
    pub fn read_metadata(&mut self) -> io::Result<String> {
        let offset = self.metadata_offset();
        let length = self.metadata_length();
        self.file.seek(SeekFrom::Start(offset))?;
        #[allow(clippy::cast_possible_truncation)]
        let mut compressed = vec![0u8; length as usize];
        self.file.read_exact(&mut compressed)?;

        if self.internal_compression == 2 {
            let buf = gzip_decompress(&compressed)?;
            Ok(String::from_utf8_lossy(&buf).to_string())
        } else {
            Ok(String::from_utf8_lossy(&compressed).to_string())
        }
    }

    /// Read all tile entries by traversing root + leaf directories.
    pub fn read_all_entries(&mut self) -> io::Result<Vec<TileEntry>> {
        let root_entries = self.read_directory(self.root_dir_offset, self.root_dir_length)?;
        let mut all_entries = Vec::new();

        for entry in &root_entries {
            if entry.run_length == 0 {
                let leaf_offset = self.leaf_dirs_offset + entry.offset;
                #[allow(clippy::cast_possible_truncation)]
                let leaf_entries = self.read_directory(leaf_offset, u64::from(entry.length))?;
                expand_entries(&leaf_entries, &mut all_entries);
            } else {
                expand_single(entry, &mut all_entries);
            }
        }

        Ok(all_entries)
    }

    /// Read and decode a single directory section.
    pub fn read_directory(
        &mut self,
        offset: u64,
        length: u64,
    ) -> io::Result<Vec<RawDirEntry>> {
        self.file.seek(SeekFrom::Start(offset))?;
        #[allow(clippy::cast_possible_truncation)]
        let mut compressed = vec![0u8; length as usize];
        self.file.read_exact(&mut compressed)?;

        let raw = if self.internal_compression == 2 {
            gzip_decompress(&compressed)?
        } else {
            compressed
        };

        decode_directory(&raw)
    }

    /// Read and decompress a single tile's payload.
    pub fn read_tile(&mut self, entry: &TileEntry) -> io::Result<Vec<u8>> {
        let abs_offset = self.data_offset + entry.offset;
        self.file.seek(SeekFrom::Start(abs_offset))?;
        #[allow(clippy::cast_possible_truncation)]
        let mut compressed = vec![0u8; entry.length as usize];
        self.file.read_exact(&mut compressed)?;
        gzip_decompress(&compressed)
    }

    /// Read raw compressed tile bytes without decompressing.
    pub fn read_tile_raw(&mut self, entry: &TileEntry) -> io::Result<Vec<u8>> {
        let abs_offset = self.data_offset + entry.offset;
        self.file.seek(SeekFrom::Start(abs_offset))?;
        #[allow(clippy::cast_possible_truncation)]
        let mut buf = vec![0u8; entry.length as usize];
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }
}

// ---------------------------------------------------------------------------
// Directory decoding
// ---------------------------------------------------------------------------

pub fn expand_entries(dir_entries: &[RawDirEntry], out: &mut Vec<TileEntry>) {
    for e in dir_entries {
        if e.run_length == 0 {
            continue;
        }
        expand_single(e, out);
    }
}

pub fn expand_single(e: &RawDirEntry, out: &mut Vec<TileEntry>) {
    for r in 0..e.run_length {
        out.push(TileEntry {
            tile_id: e.tile_id + u64::from(r),
            offset: e.offset,
            length: e.length,
        });
    }
}

/// Decode a PMTiles v3 directory from its columnar varint encoding.
pub fn decode_directory(data: &[u8]) -> io::Result<Vec<RawDirEntry>> {
    let mut c = Cursor::new(data);

    #[allow(clippy::cast_possible_truncation)]
    let count = c
        .read_varint()
        .map_err(|e| io::Error::other(format!("directory count: {e}")))?
        as usize;

    let mut tile_ids = Vec::with_capacity(count);
    let mut prev: u64 = 0;
    for _ in 0..count {
        let delta = c
            .read_varint()
            .map_err(|e| io::Error::other(format!("tile_id delta: {e}")))?;
        prev += delta;
        tile_ids.push(prev);
    }

    let mut run_lengths = Vec::with_capacity(count);
    for _ in 0..count {
        #[allow(clippy::cast_possible_truncation)]
        let v = c
            .read_varint()
            .map_err(|e| io::Error::other(format!("run_length: {e}")))? as u32;
        run_lengths.push(v);
    }

    let mut lengths = Vec::with_capacity(count);
    for _ in 0..count {
        #[allow(clippy::cast_possible_truncation)]
        let v = c
            .read_varint()
            .map_err(|e| io::Error::other(format!("length: {e}")))? as u32;
        lengths.push(v);
    }

    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let v = c
            .read_varint()
            .map_err(|e| io::Error::other(format!("offset: {e}")))?;
        let offset = if v == 0 {
            if i == 0 {
                return Err(io::Error::other(
                    "offset: zero sentinel is invalid for first entry",
                ));
            }
            let prev_entry: &RawDirEntry = &entries[i - 1];
            prev_entry
                .offset
                .checked_add(u64::from(prev_entry.length))
                .ok_or_else(|| io::Error::other("offset: contiguous addition overflow"))?
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

    Ok(entries)
}

// ---------------------------------------------------------------------------
// MVT layer decoder (minimal)
// ---------------------------------------------------------------------------

pub struct MvtLayer {
    pub name: String,
    pub feature_count: usize,
}

/// Decode the top-level MVT tile structure to extract layer names and feature counts.
pub fn decode_mvt_layers(data: &[u8]) -> io::Result<Vec<MvtLayer>> {
    let mut layers = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = cursor.read_tag() {
        if field == 3 && wire_type == WIRE_LEN {
            let sub = cursor
                .read_len_delimited()
                .map_err(|e| io::Error::other(format!("mvt layer: {e}")))?;
            layers.push(decode_mvt_layer(sub));
        } else {
            cursor
                .skip_field(wire_type)
                .map_err(|e| io::Error::other(format!("mvt skip: {e}")))?;
        }
    }
    Ok(layers)
}

fn decode_mvt_layer(data: &[u8]) -> MvtLayer {
    let mut layer = MvtLayer {
        name: String::new(),
        feature_count: 0,
    };
    let mut cursor = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = cursor.read_tag() {
        if wire_type == WIRE_LEN {
            if let Ok(sub) = cursor.read_len_delimited() {
                match field {
                    1 => layer.name = String::from_utf8_lossy(sub).to_string(),
                    2 => layer.feature_count += 1,
                    _ => {}
                }
            }
        } else if cursor.skip_field(wire_type).is_err() {
            break;
        }
    }
    layer
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::decode_directory;
    use protohoggr::encode_varint;

    fn encode_directory_raw(
        tile_deltas: &[u64],
        run_lengths: &[u64],
        lengths: &[u64],
        offsets: &[u64],
    ) -> Vec<u8> {
        assert_eq!(tile_deltas.len(), run_lengths.len());
        assert_eq!(tile_deltas.len(), lengths.len());
        assert_eq!(tile_deltas.len(), offsets.len());
        let mut out = Vec::new();
        encode_varint(&mut out, tile_deltas.len() as u64);
        for &v in tile_deltas {
            encode_varint(&mut out, v);
        }
        for &v in run_lengths {
            encode_varint(&mut out, v);
        }
        for &v in lengths {
            encode_varint(&mut out, v);
        }
        for &v in offsets {
            encode_varint(&mut out, v);
        }
        out
    }

    #[test]
    fn decode_directory_rejects_truncated_stream() {
        // count=1 and one tile_id delta, but missing remaining columns.
        let mut raw = Vec::new();
        encode_varint(&mut raw, 1);
        encode_varint(&mut raw, 5);
        let err = decode_directory(&raw)
            .err()
            .expect("should fail on truncated directory");
        assert!(
            err.to_string().contains("run_length")
                || err.to_string().contains("length")
                || err.to_string().contains("offset")
        );
    }

    #[test]
    fn decode_directory_rejects_zero_offset_for_first_entry() {
        let raw = encode_directory_raw(&[5], &[1], &[10], &[0]);
        let err = decode_directory(&raw)
            .err()
            .expect("first entry offset sentinel must fail");
        assert!(err.to_string().contains("invalid for first entry"));
    }

    #[test]
    fn decode_directory_rejects_contiguous_offset_overflow() {
        // Entry 0: explicit offset v=u64::MAX => offset=u64::MAX-1, length=10.
        // Entry 1: contiguous sentinel (v=0) => (u64::MAX-1)+10 overflows.
        let raw = encode_directory_raw(&[5, 1], &[1, 1], &[10, 1], &[u64::MAX, 0]);
        let err = decode_directory(&raw)
            .err()
            .expect("contiguous offset overflow should fail");
        assert!(err.to_string().contains("overflow"));
    }
}
