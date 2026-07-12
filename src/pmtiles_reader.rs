//! Minimal PMTiles v3 reader.
//!
//! Provides enough functionality to open a PMTiles archive, traverse its
//! directory entries, read individual tiles, and decode MVT layer structure.
//! Used by the `verify` subcommand and integration tests.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use memmap2::{Mmap, MmapOptions};

use flate2::read::GzDecoder;
use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileEntry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawDirEntry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

/// A reference to one stored tile payload. Multiple directory entries may
/// point at the same blob.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct BlobRef {
    pub offset: u64,
    pub length: u32,
}

/// Memory-mapped PMTiles view for workloads which repeatedly inspect raw tile
/// blobs. Unlike `PmtilesReader`, reads borrow the map and allocate nothing.
pub struct ArchiveView {
    map: Mmap,
    header: [u8; HEADER_SIZE],
    root_dir_offset: u64,
    root_dir_length: u64,
    leaf_dirs_offset: u64,
    data_offset: u64,
    internal_compression: u8,
}

impl ArchiveView {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        // SAFETY: mapping is read-only, retained by self, and every public
        // slice is bounds checked before it is exposed.
        let map = unsafe { MmapOptions::new().map(&file)? };
        if map.len() < HEADER_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "archive shorter than PMTiles header",
            ));
        }
        let mut header = [0_u8; HEADER_SIZE];
        header.copy_from_slice(&map[..HEADER_SIZE]);
        if &header[..7] != b"PMTiles" || header[7] != 3 {
            return Err(io::Error::other("not PMTiles v3"));
        }
        if header[97] != 1 && header[97] != 2 {
            return Err(io::Error::other("unsupported PMTiles internal compression"));
        }
        if header[99] != 1 {
            return Err(io::Error::other("ArchiveView requires MVT tiles"));
        }
        if header[98] != 2 {
            return Err(io::Error::other(
                "ArchiveView requires gzip tile compression",
            ));
        }
        Ok(Self {
            root_dir_offset: read_u64_le(&header, 8),
            root_dir_length: read_u64_le(&header, 16),
            leaf_dirs_offset: read_u64_le(&header, 40),
            data_offset: read_u64_le(&header, 56),
            internal_compression: header[97],
            map,
            header,
        })
    }

    pub fn header(&self) -> &[u8; HEADER_SIZE] {
        &self.header
    }
    pub fn min_zoom(&self) -> u8 {
        self.header[100]
    }
    pub fn max_zoom(&self) -> u8 {
        self.header[101]
    }
    pub fn tile_type(&self) -> u8 {
        self.header[99]
    }
    pub fn tile_compression(&self) -> u8 {
        self.header[98]
    }
    pub fn num_addressed(&self) -> u64 {
        read_u64_le(&self.header, 72)
    }

    fn slice(&self, offset: u64, length: u64, label: &str) -> io::Result<&[u8]> {
        let end = offset
            .checked_add(length)
            .ok_or_else(|| io::Error::other(format!("{label} range overflow")))?;
        let start = usize::try_from(offset)
            .map_err(|_| io::Error::other(format!("{label} offset too large")))?;
        let end =
            usize::try_from(end).map_err(|_| io::Error::other(format!("{label} end too large")))?;
        self.map.get(start..end).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{label} outside archive"),
            )
        })
    }

    fn directory(&self, offset: u64, length: u64) -> io::Result<Vec<RawDirEntry>> {
        let raw = self.slice(offset, length, "directory")?;
        if self.internal_compression == 2 {
            decode_directory(&gzip_decompress(raw)?)
        } else {
            decode_directory(raw)
        }
    }

    pub fn read_all_runs(&self) -> io::Result<Vec<RawDirEntry>> {
        let root = self.directory(self.root_dir_offset, self.root_dir_length)?;
        let mut runs = Vec::new();
        for entry in root {
            if entry.run_length == 0 {
                let offset = self
                    .leaf_dirs_offset
                    .checked_add(entry.offset)
                    .ok_or_else(|| io::Error::other("leaf directory offset overflow"))?;
                runs.extend(
                    self.directory(offset, u64::from(entry.length))?
                        .into_iter()
                        .filter(|run| run.run_length != 0),
                );
            } else {
                runs.push(entry);
            }
        }
        runs.sort_unstable_by_key(|entry| entry.tile_id);
        check_run_invariants(&runs)?;
        Ok(runs)
    }

    pub fn raw_blob(&self, blob: BlobRef) -> io::Result<&[u8]> {
        self.raw_blob_at(blob.offset, blob.length)
    }

    pub fn raw_blob_at(&self, offset: u64, length: u32) -> io::Result<&[u8]> {
        let offset = self
            .data_offset
            .checked_add(offset)
            .ok_or_else(|| io::Error::other("tile data offset overflow"))?;
        self.slice(offset, u64::from(length), "tile payload")
    }

    pub fn metadata(&self) -> io::Result<String> {
        let compressed = self.slice(
            read_u64_le(&self.header, 24),
            read_u64_le(&self.header, 32),
            "metadata",
        )?;
        let raw = if self.internal_compression == 2 {
            gzip_decompress(compressed)?
        } else {
            compressed.to_vec()
        };
        String::from_utf8(raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
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

    /// Read all tile runs by traversing root and leaf directories.
    ///
    /// The returned entries are sorted by tile ID and retain PMTiles run
    /// lengths, so callers do not need memory proportional to addressed tiles.
    pub fn read_all_runs(&mut self) -> io::Result<Vec<RawDirEntry>> {
        let root_entries = self.read_directory(self.root_dir_offset, self.root_dir_length)?;
        let mut runs = Vec::new();

        for entry in &root_entries {
            if entry.run_length == 0 {
                let leaf_offset = self
                    .leaf_dirs_offset
                    .checked_add(entry.offset)
                    .ok_or_else(|| io::Error::other("leaf directory offset overflow"))?;
                #[allow(clippy::cast_possible_truncation)]
                let leaf_entries = self.read_directory(leaf_offset, u64::from(entry.length))?;
                runs.extend(
                    leaf_entries
                        .into_iter()
                        .filter(|entry| entry.run_length != 0),
                );
            } else {
                runs.push(*entry);
            }
        }

        runs.sort_unstable_by_key(|entry| entry.tile_id);
        check_run_invariants(&runs)?;
        Ok(runs)
    }

    /// Read and decode a single directory section.
    pub fn read_directory(&mut self, offset: u64, length: u64) -> io::Result<Vec<RawDirEntry>> {
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

/// Sorted runs must not overlap and their ends must not overflow, or
/// `find_entry`'s predecessor-only probe would silently miss covered tiles.
/// A malformed archive becomes a container error instead.
fn check_run_invariants(runs: &[RawDirEntry]) -> io::Result<()> {
    let mut prev_end = 0u64;
    for run in runs {
        if run.tile_id < prev_end {
            return Err(io::Error::other("tile runs overlap"));
        }
        prev_end = run
            .tile_id
            .checked_add(u64::from(run.run_length))
            .ok_or_else(|| io::Error::other("tile run end overflows"))?;
    }
    Ok(())
}

/// Find one addressed tile in a sorted list of non-overlapping PMTiles
/// directory runs (the `read_all_runs` invariant).
pub fn find_entry(runs: &[RawDirEntry], tile_id: u64) -> Option<TileEntry> {
    let index = runs.partition_point(|run| run.tile_id <= tile_id);
    let run = runs.get(index.checked_sub(1)?)?;
    let run_end = run.tile_id.checked_add(u64::from(run.run_length))?;
    if tile_id < run_end {
        Some(TileEntry {
            tile_id,
            offset: run.offset,
            length: run.length,
        })
    } else {
        None
    }
}

/// Decode a PMTiles v3 directory from its columnar varint encoding.
pub fn decode_directory(data: &[u8]) -> io::Result<Vec<RawDirEntry>> {
    let mut c = Cursor::new(data);

    #[allow(clippy::cast_possible_truncation)]
    let count = c
        .read_varint()
        .map_err(|e| io::Error::other(format!("directory count: {e}")))? as usize;

    let mut tile_ids = Vec::with_capacity(count);
    let mut prev: u64 = 0;
    for _ in 0..count {
        let delta = c
            .read_varint()
            .map_err(|e| io::Error::other(format!("tile_id delta: {e}")))?;
        prev = prev
            .checked_add(delta)
            .ok_or_else(|| io::Error::other("tile_id delta: cumulative overflow"))?;
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
    pub points: usize,
    pub lines: usize,
    pub polygons: usize,
    pub keys: Vec<String>,
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
        points: 0,
        lines: 0,
        polygons: 0,
        keys: Vec::new(),
    };
    let mut features: Vec<&[u8]> = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = cursor.read_tag() {
        if wire_type == WIRE_LEN {
            if let Ok(sub) = cursor.read_len_delimited() {
                match field {
                    1 => layer.name = String::from_utf8_lossy(sub).to_string(),
                    2 => features.push(sub),
                    3 => layer.keys.push(String::from_utf8_lossy(sub).to_string()),
                    _ => {}
                }
            }
        } else if cursor.skip_field(wire_type).is_err() {
            break;
        }
    }
    layer.feature_count = features.len();
    for feat_data in features {
        let mut fc = Cursor::new(feat_data);
        while let Ok(Some((ff, fw))) = fc.read_tag() {
            if ff == 3 && fw == WIRE_VARINT {
                if let Ok(gt) = fc.read_varint() {
                    match gt {
                        1 => layer.points += 1,
                        2 => layer.lines += 1,
                        3 => layer.polygons += 1,
                        _ => {}
                    }
                }
            } else if fc.skip_field(fw).is_err() {
                break;
            }
        }
    }
    layer
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{RawDirEntry, decode_directory, find_entry};
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
        let err = decode_directory(&raw).expect_err("should fail on truncated directory");
        assert!(
            err.to_string().contains("run_length")
                || err.to_string().contains("length")
                || err.to_string().contains("offset")
        );
    }

    #[test]
    fn decode_directory_rejects_zero_offset_for_first_entry() {
        let raw = encode_directory_raw(&[5], &[1], &[10], &[0]);
        let err = decode_directory(&raw).expect_err("first entry offset sentinel must fail");
        assert!(err.to_string().contains("invalid for first entry"));
    }

    #[test]
    fn decode_directory_rejects_contiguous_offset_overflow() {
        // Entry 0: explicit offset v=u64::MAX => offset=u64::MAX-1, length=10.
        // Entry 1: contiguous sentinel (v=0) => (u64::MAX-1)+10 overflows.
        let raw = encode_directory_raw(&[5, 1], &[1, 1], &[10, 1], &[u64::MAX, 0]);
        let err = decode_directory(&raw).expect_err("contiguous offset overflow should fail");
        assert!(err.to_string().contains("overflow"));
    }

    #[test]
    fn decode_directory_rejects_tile_id_delta_overflow() {
        // Entry 0 sets cumulative tile_id to u64::MAX; entry 1 overflows by +1.
        let raw = encode_directory_raw(&[u64::MAX, 1], &[1, 1], &[1, 1], &[1, 2]);
        let err = decode_directory(&raw).expect_err("tile_id overflow should fail");
        assert!(err.to_string().contains("tile_id delta"));
        assert!(err.to_string().contains("overflow"));
    }

    #[test]
    fn run_invariants_reject_overlap_and_overflow() {
        let overlapping = [
            RawDirEntry {
                tile_id: 10,
                offset: 100,
                length: 7,
                run_length: 3,
            },
            RawDirEntry {
                tile_id: 12,
                offset: 200,
                length: 9,
                run_length: 1,
            },
        ];
        let err = super::check_run_invariants(&overlapping).expect_err("overlap must fail");
        assert!(err.to_string().contains("overlap"));

        let overflowing = [RawDirEntry {
            tile_id: u64::MAX,
            offset: 100,
            length: 7,
            run_length: 2,
        }];
        let err = super::check_run_invariants(&overflowing).expect_err("overflow must fail");
        assert!(err.to_string().contains("overflow"));

        let adjacent = [
            RawDirEntry {
                tile_id: 10,
                offset: 100,
                length: 7,
                run_length: 3,
            },
            RawDirEntry {
                tile_id: 13,
                offset: 200,
                length: 9,
                run_length: 1,
            },
        ];
        super::check_run_invariants(&adjacent).expect("adjacent runs are valid");
    }

    #[test]
    fn find_entry_covers_run_interiors_boundaries_and_misses() {
        let runs = [
            RawDirEntry {
                tile_id: 10,
                offset: 100,
                length: 7,
                run_length: 3,
            },
            RawDirEntry {
                tile_id: 20,
                offset: 200,
                length: 9,
                run_length: 2,
            },
        ];

        let first = find_entry(&runs, 10).expect("first run boundary");
        let interior = find_entry(&runs, 11).expect("run interior");
        let last = find_entry(&runs, 12).expect("last tile in first run");
        let second = find_entry(&runs, 20).expect("second run boundary");
        assert_eq!(
            (first.offset, interior.offset, last.offset),
            (100, 100, 100)
        );
        assert_eq!(second.length, 9);
        assert!(find_entry(&runs, 9).is_none());
        assert!(find_entry(&runs, 13).is_none());
        assert!(find_entry(&runs, 22).is_none());
    }
}
