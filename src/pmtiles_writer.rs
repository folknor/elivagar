//! PMTiles v3 archive writer.
//!
//! Writes clustered, gzip-compressed PMTiles archives with Hilbert-ordered
//! tile IDs and content deduplication. Supports both in-memory and streaming
//! modes for the tile blob and directory entries.
//!
//! # Archive layout
//!
//! Sections are laid out as `[header][root dir][zeros to 16384][tile data]
//! [metadata][leaf dirs]` - tile data at a fixed offset directly after a
//! reserved 16 KiB init section, directories that depend on the full tile
//! set appended AFTER the data. PMTiles readers locate every section through
//! the header's offset/length fields, so section order is a producer choice;
//! this is the same layout planetiler ships, chosen so streaming mode can
//! write tile data straight into the destination file while tiles arrive.
//! The previous layout (`[header][root][metadata][leaf dirs][tile data]`)
//! forced the whole data section through a temp file and an end-of-run copy
//! into the archive - 14 GB read + 14 GB written again, ~19s, on a
//! north-america run - because the leaf-dir size is unknown until the last
//! tile has been added.
//!
//! The reserved init section is the spec's constraint that the root
//! directory must live in the first 16,384 bytes; `finalize_directories`
//! grows the leaf fanout until the compressed root fits. The fixed 16,384
//! data offset also keeps the data section 4 KiB-aligned for O_DIRECT
//! serving.
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

use rustc_hash::FxHashMap;
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufReader, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
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

/// Tile payload format stored in PMTiles tile data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileDataFormat {
    Mvt,
    Mlt,
}

impl TileDataFormat {
    fn header_tile_type(self) -> u8 {
        match self {
            Self::Mvt => 1, // MVT
            Self::Mlt => 0, // Unknown in PMTiles header; explicit metadata carries mlt marker.
        }
    }

    fn metadata_format(self) -> &'static str {
        match self {
            Self::Mvt => "pbf",
            Self::Mlt => "mlt",
        }
    }

    fn metadata_payload(self) -> &'static str {
        match self {
            Self::Mvt => "mvt",
            Self::Mlt => "mlt",
        }
    }
}

/// Compression mode for tile payload blobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileDataCompression {
    None,
    Gzip,
    Brotli,
    Zstd,
}

impl TileDataCompression {
    fn header_value(self) -> u8 {
        match self {
            Self::None => 1,
            Self::Gzip => 2,
            Self::Brotli => 3,
            Self::Zstd => 4,
        }
    }

    fn metadata_value(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Gzip => "gzip",
            Self::Brotli => "brotli",
            Self::Zstd => "zstd",
        }
    }
}

/// Maximum number of entries in the dedup map before we stop inserting.
/// At planet scale, unlimited dedup grows to ~7 GB. Capping at 1M entries
/// keeps the map under ~60 MB while still deduplicating the ocean fill tiles
/// (which are added early and remain cached).
const MAX_DEDUP_ENTRIES: usize = 1_000_000;

/// Fixed salt for the secondary fingerprint hasher. Domain-separates the two
/// SipHash passes so they produce different outputs for the same input.
const DEDUP_FP2_SALT: u64 = 0xA5A5_A5A5_A5A5_A5A5;

/// Reserved prologue: header + root directory + zero padding. Tile data
/// starts exactly here. 16,384 is the spec's root-directory window (the root
/// must be contained in the first 16,384 bytes) and is 4 KiB-aligned, which
/// preserves the O_DIRECT-friendly data alignment the old layout computed
/// dynamically.
const INIT_SECTION: u64 = 16384;

/// Compressed root-directory byte budget inside the init section.
#[allow(clippy::cast_possible_truncation)]
const MAX_ROOT_BYTES: usize = INIT_SECTION as usize - 127;

// ---------------------------------------------------------------------------
// Dedup statistics
// ---------------------------------------------------------------------------

/// Counters tracking deduplication behavior for observability.
#[derive(Debug, Default, Clone)]
pub struct DedupStats {
    /// Number of hash-map lookups that found a matching primary hash.
    pub candidates: u64,
    /// Number of tiles successfully deduplicated (hash + length + fingerprint all matched).
    pub tiles_reused: u64,
    /// Cumulative compressed bytes saved by dedup.
    pub bytes_saved: u64,
    /// Hash matched but compressed length differed.
    pub reject_len_mismatch: u64,
    /// Hash and length matched but secondary fingerprint differed.
    pub reject_fp_mismatch: u64,
    /// Tiles that bypassed dedup insertion because the map hit its cap.
    pub insert_skipped_cap: u64,
    /// Number of times a new entry was added to a bucket that already had entries
    /// (different tiles sharing the same primary hash).
    pub hash_bucket_collisions: u64,
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

/// Tile data storage: in-memory or streamed to disk.
/// Streaming mode avoids buffering all compressed tile data in RAM (~3 GB
/// for a planet). The blob file IS the destination archive under a
/// `.partial` name, pre-seeded with the reserved init section, so tile data
/// lands at its final offset as it arrives and `write_to()` only appends the
/// trailing sections and patches the prologue - no end-of-run data copy.
enum TileBlob {
    /// All tile data in a Vec (original behavior, for tests and small runs).
    Memory(Vec<u8>),
    /// Tile data streamed to `<output>.partial` starting at INIT_SECTION.
    /// `offset` is relative to the data section start.
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
/// Duplicate tile contents are automatically deduplicated using dual SipHash
/// fingerprints plus compressed length matching.
///
/// Two storage modes are available:
/// - [`new()`](Self::new) - in-memory (tile data in a `Vec`). Best for small
///   extracts and tests.
/// - [`new_streaming()`](Self::new_streaming) - file-backed (tile data and
///   directory entries streamed to disk). Required for planet-scale runs to
///   avoid multi-GB RAM usage.
pub struct PmtilesWriter {
    config: PmtilesConfig,
    tile_data_format: TileDataFormat,
    tile_data_compression: TileDataCompression,
    source_pbf_filename: Option<String>,
    osmosis_replication_timestamp: Option<i64>,
    metadata_extensions: Vec<String>,
    ocean_only_metadata: bool,
    /// Exact metadata supplied by an archive-preserving rewrite.  This is
    /// intentionally narrow: normal production writes always compose metadata.
    metadata_verbatim: Option<String>,
    /// Concatenated compressed tile data (in-memory or file-backed).
    blob: TileBlob,
    /// Total number of tiles addressed (including deduped references).
    num_addressed: u64,
    /// Current run being built (flushed when a new non-extending tile arrives).
    current_run: Option<DirEntry>,
    /// Directory entries (in-memory or streamed to disk).
    dir_store: DirStore,
    /// Primary hash → Vec<(offset, length, fingerprint2)> for dedup.
    /// Bucketed so primary-hash collisions don't silently evict entries.
    dedup: FxHashMap<u64, Vec<(u64, u32, u64)>>,
    /// Total entries across all dedup buckets.
    dedup_count: usize,
    /// Maximum dedup entries (defaults to MAX_DEDUP_ENTRIES).
    dedup_cap: usize,
    /// Number of unique tile contents (after dedup).
    unique_count: u64,
    /// Dedup behavior counters.
    dedup_stats: DedupStats,
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

    /// Dedup behavior counters for observability.
    pub fn dedup_stats(&self) -> &DedupStats {
        &self.dedup_stats
    }

    /// Set source PBF filename to include in PMTiles metadata JSON.
    pub fn set_source_pbf_filename(&mut self, filename: impl Into<String>) {
        self.source_pbf_filename = Some(filename.into());
    }

    /// Set OSM replication timestamp (seconds since UNIX epoch) for metadata JSON.
    pub fn set_osmosis_replication_timestamp(&mut self, ts: i64) {
        self.osmosis_replication_timestamp = Some(ts);
    }

    /// Set tile payload format/compression contract written into PMTiles header + metadata.
    pub fn set_tile_contract(
        &mut self,
        tile_data_format: TileDataFormat,
        tile_data_compression: TileDataCompression,
    ) {
        self.tile_data_format = tile_data_format;
        self.tile_data_compression = tile_data_compression;
    }

    /// Add a trusted JSON member to archive metadata. The caller owns the
    /// schema; this keeps PMTiles metadata extensible without changing the
    /// ordinary Shortbread contract.
    ///
    /// Members accumulate in call order. Several independent schemas share
    /// this hook - `ocean_artifact` and `elivagar` at minimum - and each must
    /// stay a distinct top-level key: `ocean::OceanArtifactKey::from_json`
    /// reads `ocean_artifact` at the top level, so a mechanism that let one
    /// member displace another would silently invalidate every durable ocean
    /// artifact already built.
    pub fn add_metadata_member(&mut self, json_member: impl Into<String>) {
        self.metadata_extensions.push(json_member.into());
    }

    /// Write an ocean-only vector layer declaration for the durable artifact.
    pub fn set_ocean_only_metadata(&mut self) {
        self.ocean_only_metadata = true;
    }

    /// Preserve an existing metadata document during an archive rewrite.
    ///
    /// The corpus mutation instrument uses this so its output remains under
    /// exactly the source archive's provenance contract.
    pub fn set_metadata_verbatim(&mut self, json: String) {
        self.metadata_verbatim = Some(json);
    }
}

impl PmtilesWriter {
    /// Create an in-memory writer (tile data kept in a Vec).
    pub fn new(config: PmtilesConfig) -> Self {
        PmtilesWriter {
            config,
            tile_data_format: TileDataFormat::Mvt,
            tile_data_compression: TileDataCompression::Gzip,
            source_pbf_filename: None,
            osmosis_replication_timestamp: None,
            metadata_extensions: Vec::new(),
            ocean_only_metadata: false,
            metadata_verbatim: None,
            blob: TileBlob::Memory(Vec::new()),
            num_addressed: 0,
            current_run: None,
            dir_store: DirStore::Memory(Vec::new()),
            dedup: FxHashMap::default(),
            dedup_count: 0,
            dedup_cap: MAX_DEDUP_ENTRIES,
            unique_count: 0,
            dedup_stats: DedupStats::default(),
        }
    }

    /// Create a streaming writer. Tile data is written directly into
    /// `<output_path>.partial` at its final archive offset; `write_to()`
    /// completes the prologue and renames it to `output_path`. Directory
    /// entries stream to a temp file in `tmp_dir` as before.
    ///
    /// # Errors
    /// Returns `io::Error` if creating either file fails.
    pub fn new_streaming(
        config: PmtilesConfig,
        tmp_dir: &Path,
        output_path: &Path,
    ) -> io::Result<Self> {
        let mut partial = output_path.as_os_str().to_owned();
        partial.push(".partial");
        let blob_path = PathBuf::from(partial);
        let file = File::create(&blob_path)?;
        let mut writer = BufWriter::with_capacity(1 << 20, file); // 1 MB buffer
        // Reserve the init section (header + root + zeros); the zeros between
        // the root's end and INIT_SECTION are written now so write_to() never
        // has to pad, only to overwrite the front.
        #[allow(clippy::cast_possible_truncation)]
        writer.write_all(&vec![0_u8; INIT_SECTION as usize])?;

        let dir_path = tmp_dir.join("dir_entries.bin");
        let dir_file = File::create(&dir_path)?;
        let dir_writer = BufWriter::with_capacity(1 << 16, dir_file);

        Ok(PmtilesWriter {
            config,
            tile_data_format: TileDataFormat::Mvt,
            tile_data_compression: TileDataCompression::Gzip,
            source_pbf_filename: None,
            osmosis_replication_timestamp: None,
            metadata_extensions: Vec::new(),
            ocean_only_metadata: false,
            metadata_verbatim: None,
            blob: TileBlob::File {
                writer,
                path: blob_path,
                offset: 0,
            },
            num_addressed: 0,
            current_run: None,
            dir_store: DirStore::Streaming {
                writer: dir_writer,
                path: dir_path,
                count: 0,
            },
            dedup: FxHashMap::default(),
            dedup_count: 0,
            dedup_cap: MAX_DEDUP_ENTRIES,
            unique_count: 0,
            dedup_stats: DedupStats::default(),
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
        if data.len() > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tile data exceeds 4 GB",
            ));
        }
        let tile_id = xy_to_tile_id(z, x, y);

        // Dual-fingerprint dedup: two salted SipHash passes plus length check.
        // Dramatically reduces false-dedup risk compared to single hash + length.
        let mut h1 = DefaultHasher::new();
        data.hash(&mut h1);
        let hash1 = h1.finish();

        let mut h2 = DefaultHasher::new();
        DEDUP_FP2_SALT.hash(&mut h2);
        data.hash(&mut h2);
        let hash2 = h2.finish();

        let data_len = data.len() as u32;

        if let Some(candidates) = self.dedup.get(&hash1) {
            self.dedup_stats.candidates += 1;
            let mut matched = false;
            for &(dup_offset, dup_length, dup_fp2) in candidates {
                if dup_length == data_len {
                    if dup_fp2 == hash2 {
                        // All three match: primary hash, length, secondary fingerprint.
                        self.dedup_stats.tiles_reused += 1;
                        self.dedup_stats.bytes_saved += u64::from(dup_length);
                        self.push_dir_entry(tile_id, dup_offset, dup_length)?;
                        matched = true;
                        break;
                    }
                    self.dedup_stats.reject_fp_mismatch += 1;
                } else {
                    self.dedup_stats.reject_len_mismatch += 1;
                }
            }
            if matched {
                return Ok(false);
            }
        }

        let offset;
        match &mut self.blob {
            TileBlob::Memory(vec) => {
                offset = vec.len() as u64;
                vec.extend_from_slice(data);
            }
            TileBlob::File {
                writer,
                offset: file_offset,
                ..
            } => {
                offset = *file_offset;
                writer.write_all(data)?;
                *file_offset += data.len() as u64;
            }
        }

        if self.dedup_count < self.dedup_cap {
            let bucket = self.dedup.entry(hash1).or_default();
            if !bucket.is_empty() {
                self.dedup_stats.hash_bucket_collisions += 1;
            }
            bucket.push((offset, data_len, hash2));
            self.dedup_count += 1;
        } else {
            self.dedup_stats.insert_skipped_cap += 1;
        }
        self.push_dir_entry(tile_id, offset, data_len)?;
        self.unique_count += 1;
        Ok(true)
    }

    /// Add a consecutive run sharing one compressed payload. This deliberately
    /// preserves intra-run sharing even after the global dedup insertion cap.
    pub fn add_run(&mut self, tile_id: u64, run_length: u32, data: &[u8]) -> io::Result<bool> {
        if run_length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PMTiles run length must be non-zero",
            ));
        }
        if data.len() > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tile data exceeds 4 GB",
            ));
        }
        let data_len = u32::try_from(data.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "tile data exceeds 4 GB"))?;
        let mut h1 = DefaultHasher::new();
        data.hash(&mut h1);
        let hash1 = h1.finish();
        let mut h2 = DefaultHasher::new();
        DEDUP_FP2_SALT.hash(&mut h2);
        data.hash(&mut h2);
        let hash2 = h2.finish();
        let existing = self.dedup.get(&hash1).and_then(|candidates| {
            self.dedup_stats.candidates += 1;
            candidates.iter().find_map(|&(offset, length, fp)| {
                (length == data_len && fp == hash2).then_some((offset, length))
            })
        });
        let (offset, stored) = if let Some(found) = existing {
            self.dedup_stats.tiles_reused += u64::from(run_length);
            self.dedup_stats.bytes_saved += u64::from(data_len) * u64::from(run_length);
            (found.0, false)
        } else {
            let offset = match &mut self.blob {
                TileBlob::Memory(blob) => {
                    let offset = blob.len() as u64;
                    blob.extend_from_slice(data);
                    offset
                }
                TileBlob::File { writer, offset, .. } => {
                    let current = *offset;
                    writer.write_all(data)?;
                    *offset += u64::from(data_len);
                    current
                }
            };
            if self.dedup_count < self.dedup_cap {
                let bucket = self.dedup.entry(hash1).or_default();
                if !bucket.is_empty() {
                    self.dedup_stats.hash_bucket_collisions += 1;
                }
                bucket.push((offset, data_len, hash2));
                self.dedup_count += 1;
            } else {
                self.dedup_stats.insert_skipped_cap += 1;
            }
            self.unique_count += 1;
            (offset, true)
        };
        self.push_dir_run(tile_id, run_length, offset, data_len)?;
        if stored && run_length > 1 {
            self.dedup_stats.tiles_reused += u64::from(run_length - 1);
            self.dedup_stats.bytes_saved += u64::from(data_len) * u64::from(run_length - 1);
        }
        Ok(stored)
    }

    /// Complete the PMTiles archive at `path`.
    ///
    /// In-memory mode writes the whole archive in one pass. Streaming mode
    /// appends metadata and leaf directories after the already-in-place tile
    /// data, patches the header and root into the reserved init section, and
    /// renames `<output>.partial` to `path` - so the archive appears at
    /// `path` only when complete, and `path` must be on the same filesystem
    /// as the `output_path` given to [`new_streaming`](Self::new_streaming)
    /// (they are the same path in every production caller).
    ///
    /// # Errors
    /// Returns `io::Error` if file creation, directory encoding, writing, or
    /// the final rename fails.
    #[hotpath::measure]
    pub fn write_to(&mut self, path: &Path) -> io::Result<()> {
        // RAM-ledger snapshot of the dedup map at its final (largest) size,
        // taken before it is dropped. The byte figure is an estimate: bucket
        // entries are 24-byte (u64, u32, u64) tuples plus per-bucket Vec
        // headers and the map's own table, dominated by the entry payload.
        crate::debug::emit_counter_usize("pmtiles_dedup_entries", self.dedup_count);
        let dedup_bytes_est = self.dedup_count * std::mem::size_of::<(u64, u32, u64)>()
            + self.dedup.len()
                * (std::mem::size_of::<u64>() + std::mem::size_of::<Vec<(u64, u32, u64)>>());
        crate::debug::emit_counter_usize("pmtiles_dedup_bytes_est", dedup_bytes_est);

        // Free dedup map - no longer needed after all tiles are added.
        drop(std::mem::take(&mut self.dedup));

        // Build directories: streaming mode reads entries from temp file in
        // LEAF_SIZE chunks (O(1) memory), in-memory mode collects all entries.
        let (root_bytes, leaf_bytes, num_entries) = self.finalize_directories()?;
        crate::debug::emit_counter_u64("pmtiles_dir_entries", num_entries);
        crate::debug::emit_counter_usize("pmtiles_root_dir_bytes", root_bytes.len());
        crate::debug::emit_counter_usize("pmtiles_leaf_dirs_bytes", leaf_bytes.len());
        let metadata_json = self.metadata_verbatim.clone().unwrap_or_else(|| {
            build_metadata(
                &self.config,
                self.tile_data_format,
                self.tile_data_compression,
                self.source_pbf_filename.as_deref(),
                self.osmosis_replication_timestamp,
                &self.metadata_extensions,
                self.ocean_only_metadata,
            )
        });
        let metadata_compressed = gzip_compress(metadata_json.as_bytes())?;

        // Clean up streaming dir_entries temp file if it exists.
        // Best-effort cleanup of streaming temp file - failure is harmless.
        if let DirStore::Streaming { path: dir_path, .. } = &self.dir_store {
            drop(std::fs::remove_file(dir_path));
        }

        // Determine tile data length.
        let data_length = match &self.blob {
            TileBlob::Memory(vec) => vec.len() as u64,
            TileBlob::File { offset, .. } => *offset,
        };

        // Layout: [header 127] [root_dir] [zeros to INIT_SECTION] [tile_data]
        // [metadata] [leaf_dirs]. finalize_directories() guarantees the root
        // fits the init section; the assert is the layout's load-bearing
        // invariant, not a recoverable condition.
        let root_dir_offset: u64 = 127;
        let root_dir_length = root_bytes.len() as u64;
        assert!(
            root_bytes.len() <= MAX_ROOT_BYTES,
            "root directory ({root_dir_length} B) exceeds the init section"
        );
        let data_offset = INIT_SECTION;
        let metadata_offset = data_offset + data_length;
        let metadata_length = metadata_compressed.len() as u64;
        let leaf_dirs_offset = metadata_offset + metadata_length;
        let leaf_dirs_length = leaf_bytes.len() as u64;

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

        match &mut self.blob {
            TileBlob::Memory(vec) => {
                // Everything is known up front: one sequential pass.
                let file = File::create(path)?;
                let mut w = BufWriter::with_capacity(1 << 20, file);
                w.write_all(&header)?;
                w.write_all(&root_bytes)?;
                w.write_all(&vec![0_u8; MAX_ROOT_BYTES - root_bytes.len()])?;
                w.write_all(vec)?;
                w.write_all(&metadata_compressed)?;
                w.write_all(&leaf_bytes)?;
                w.flush()?;
            }
            TileBlob::File {
                writer,
                path: blob_path,
                ..
            } => {
                // The tile data is already in place. Append the trailing
                // sections, overwrite the reserved prologue (the zeros
                // between root end and INIT_SECTION were written at
                // creation), and rename the partial into the archive.
                writer.flush()?;
                let mut file = File::options().write(true).open(&*blob_path)?;
                file.seek(io::SeekFrom::Start(metadata_offset))?;
                file.write_all(&metadata_compressed)?;
                file.write_all(&leaf_bytes)?;
                file.seek(io::SeekFrom::Start(0))?;
                file.write_all(&header)?;
                file.write_all(&root_bytes)?;
                drop(file);
                std::fs::rename(&*blob_path, path)?;
            }
        }
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
        if let Some(ref run) = self.current_run {
            // debug_assert only: ordering is guaranteed by the external merge sort
            // (sort key starts with tile_id). Out-of-order tiles would degrade run-length
            // compression but not produce an invalid archive - PMTiles readers binary-search
            // the directory regardless.
            debug_assert!(
                tile_id >= run.tile_id,
                "PMTiles tiles must be added in Hilbert order: got {tile_id} after {}",
                run.tile_id,
            );
        }
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
        self.current_run = Some(DirEntry {
            tile_id,
            offset,
            length,
            run_length: 1,
        });
        Ok(())
    }

    fn push_dir_run(
        &mut self,
        tile_id: u64,
        run_length: u32,
        offset: u64,
        length: u32,
    ) -> io::Result<()> {
        self.num_addressed += u64::from(run_length);
        if let Some(run) = &mut self.current_run
            && tile_id == run.tile_id + u64::from(run.run_length)
            && offset == run.offset
            && length == run.length
        {
            run.run_length = run
                .run_length
                .checked_add(run_length)
                .ok_or_else(|| io::Error::other("PMTiles run length overflow"))?;
            return Ok(());
        }
        self.flush_run()?;
        self.current_run = Some(DirEntry {
            tile_id,
            offset,
            length,
            run_length,
        });
        Ok(())
    }

    /// Collect all directory entries (flushing the current run and reading back
    /// from the streaming temp file if necessary).
    #[cfg(test)]
    #[hotpath::measure]
    fn collect_dir_entries(&mut self) -> io::Result<Vec<DirEntry>> {
        self.flush_run()?;
        match &mut self.dir_store {
            DirStore::Memory(entries) => Ok(std::mem::take(entries)),
            DirStore::Streaming {
                writer,
                path,
                count,
            } => {
                writer.flush()?;
                #[allow(clippy::cast_possible_truncation)]
                let num = *count as usize;
                let file = File::open(path)?;
                let mut reader = BufReader::with_capacity(1 << 16, file);
                read_dir_entries(&mut reader, num)
            }
        }
    }

    /// Override the dedup map capacity (test-only).
    #[cfg(test)]
    fn set_dedup_cap(&mut self, cap: usize) {
        self.dedup_cap = cap;
    }

    /// Inject a dedup entry with specific hash/fingerprint values (test-only).
    /// Used to test fingerprint rejection without needing real hash collisions.
    #[cfg(test)]
    fn inject_dedup_entry(&mut self, hash1: u64, offset: u64, length: u32, fp2: u64) {
        self.dedup
            .entry(hash1)
            .or_default()
            .push((offset, length, fp2));
        self.dedup_count += 1;
    }

    /// Compute the primary hash and secondary fingerprint for the given data (test-only).
    /// Returns (hash1, hash2).
    #[cfg(test)]
    fn compute_dedup_hashes(data: &[u8]) -> (u64, u64) {
        let mut h1 = DefaultHasher::new();
        data.hash(&mut h1);
        let hash1 = h1.finish();

        let mut h2 = DefaultHasher::new();
        DEDUP_FP2_SALT.hash(&mut h2);
        data.hash(&mut h2);
        let hash2 = h2.finish();

        (hash1, hash2)
    }

    /// Build root and leaf directory bytes from the dir store.
    ///
    /// For in-memory mode, collects entries and delegates to `build_leaf_directories`.
    /// For streaming mode, reads entries from the temp file in leaf-size
    /// chunks, building leaf directories incrementally without materializing
    /// all entries (O(1) memory vs O(n)).
    ///
    /// The compressed root MUST fit the init section (the spec's first-16,384
    /// bytes window, which the fixed data offset turns into a hard budget), so
    /// both arms retry with a doubled leaf fanout until it does. Termination:
    /// once the fanout reaches the entry count there is one leaf and the root
    /// has one entry. The retry re-reads the streaming temp file per attempt -
    /// a sequential re-read of 24 bytes per entry, paid only when the previous
    /// attempt's root overflowed ~16 KB.
    ///
    /// Returns `(root_compressed, leaf_compressed, num_entries)`.
    #[hotpath::measure]
    fn finalize_directories(&mut self) -> io::Result<(Vec<u8>, Vec<u8>, u64)> {
        const MAX_ROOT_ENTRIES: usize = 16384;
        const LEAF_SIZE: usize = 4096;

        self.flush_run()?;

        match &mut self.dir_store {
            DirStore::Memory(entries) => {
                let entries = std::mem::take(entries);
                #[allow(clippy::cast_possible_truncation)]
                let num = entries.len() as u64;
                if entries.len() <= MAX_ROOT_ENTRIES {
                    let root = gzip_compress(&encode_directory(&entries))?;
                    if root.len() <= MAX_ROOT_BYTES {
                        return Ok((root, Vec::new(), num));
                    }
                }
                let mut leaf_size = LEAF_SIZE;
                loop {
                    let (root, leaf) = build_leaf_directories(&entries, leaf_size)?;
                    if root.len() <= MAX_ROOT_BYTES {
                        return Ok((root, leaf, num));
                    }
                    leaf_size *= 2;
                }
            }
            DirStore::Streaming {
                writer,
                path,
                count,
            } => {
                writer.flush()?;
                #[allow(clippy::cast_possible_truncation)]
                let count = *count as usize;
                let path = path.clone();

                if count <= MAX_ROOT_ENTRIES {
                    // Small dataset: read all (at most 393 KB), single root
                    // directory - unless it compresses past the budget.
                    let file = File::open(&path)?;
                    let mut reader = BufReader::with_capacity(1 << 16, file);
                    let entries = read_dir_entries(&mut reader, count)?;
                    let root = gzip_compress(&encode_directory(&entries))?;
                    if root.len() <= MAX_ROOT_BYTES {
                        return Ok((root, Vec::new(), count as u64));
                    }
                    let mut leaf_size = LEAF_SIZE;
                    loop {
                        let (root, leaf) = build_leaf_directories(&entries, leaf_size)?;
                        if root.len() <= MAX_ROOT_BYTES {
                            return Ok((root, leaf, count as u64));
                        }
                        leaf_size *= 2;
                    }
                }

                let mut leaf_size = LEAF_SIZE;
                loop {
                    let (root, leaf) = build_leaf_directories_streaming(&path, count, leaf_size)?;
                    if root.len() <= MAX_ROOT_BYTES {
                        return Ok((root, leaf, count as u64));
                    }
                    leaf_size *= 2;
                }
            }
        }
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
        // Tile compression + payload type are format dependent.
        h[98] = self.tile_data_compression.header_value();
        h[99] = self.tile_data_format.header_tile_type();

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

/// Build leaf directories by streaming entries from the dir-entries temp
/// file in `leaf_size` chunks, without materializing all entries.
/// Returns (root_compressed, all_leaves_compressed).
#[hotpath::measure]
fn build_leaf_directories_streaming(
    path: &Path,
    count: usize,
    leaf_size: usize,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(1 << 16, file);

    let mut leaf_blob: Vec<u8> = Vec::new();
    let mut root_entries: Vec<DirEntry> = Vec::new();
    let mut remaining = count;

    while remaining > 0 {
        let chunk_size = remaining.min(leaf_size);
        let chunk = read_dir_entries(&mut reader, chunk_size)?;
        remaining -= chunk_size;

        let first_tile_id = chunk[0].tile_id;
        let leaf_raw = encode_directory(&chunk);
        let compressed = gzip_compress(&leaf_raw)?;

        #[allow(clippy::cast_possible_truncation)]
        let leaf_len = compressed.len() as u32;
        let leaf_offset = leaf_blob.len() as u64;
        leaf_blob.extend_from_slice(&compressed);

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

/// Build leaf directories when entries exceed the root limit.
/// Returns (root_compressed, all_leaves_compressed).
#[hotpath::measure]
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
        let compressed = gzip_compress(&leaf_raw)?;

        #[allow(clippy::cast_possible_truncation)]
        let leaf_len = compressed.len() as u32;
        let leaf_offset = leaf_blob.len() as u64;
        leaf_blob.extend_from_slice(&compressed);

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
        let delta = e.tile_id.saturating_sub(prev_tile_id);
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
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

// ---------------------------------------------------------------------------
// Metadata JSON
// ---------------------------------------------------------------------------

/// Build PMTiles metadata JSON. Hand-rolled rather than serde_json to avoid
/// a runtime dependency for ~20 lines of fixed-schema formatting.
/// Validated by test_metadata_json which round-trips through serde_json.
fn build_metadata(
    config: &PmtilesConfig,
    tile_data_format: TileDataFormat,
    tile_data_compression: TileDataCompression,
    source_pbf_filename: Option<&str>,
    osmosis_replication_timestamp: Option<i64>,
    metadata_extensions: &[String],
    ocean_only_metadata: bool,
) -> String {
    use crate::shortbread::Layer;

    let mut layer_arr = String::from("[");
    let metadata_layers: Vec<Layer> = if ocean_only_metadata {
        vec![Layer::Ocean]
    } else {
        Layer::ALL.to_vec()
    };
    for (i, layer) in metadata_layers.into_iter().enumerate() {
        if i > 0 {
            layer_arr.push(',');
        }
        let name = layer.name();
        let min_z = layer.min_zoom();
        let max_z = config.max_zoom;
        // format! is fine here - 26-iteration loop, called once per run. Cold path.
        layer_arr.push_str(&format!(
            r#"{{"id":"{name}","minzoom":{min_z},"maxzoom":{max_z}}}"#,
        ));
    }
    layer_arr.push(']');

    let mut json = format!(
        r#"{{"name":"Shortbread","format":"{}","tile_payload_format":"{}","tile_compression":"{}","type":"baselayer","minzoom":{},"maxzoom":{},"vector_layers":{layer_arr}"#,
        tile_data_format.metadata_format(),
        tile_data_format.metadata_payload(),
        tile_data_compression.metadata_value(),
        config.min_zoom,
        config.max_zoom,
    );
    if let Some(filename) = source_pbf_filename {
        json.push_str(r#","source_pbf":"#);
        json.push('"');
        json.push_str(&escape_json_string(filename));
        json.push('"');
    }
    if let Some(ts) = osmosis_replication_timestamp {
        json.push_str(&format!(r#","osmosis_replication_timestamp":{ts}"#));
    }
    for extension in metadata_extensions {
        json.push(',');
        json.push_str(extension);
    }
    json.push('}');
    json
}

fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c <= '\u{1F}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Binary helpers
// ---------------------------------------------------------------------------

/// Read `count` directory entries from a reader, 24 bytes at a time.
/// Eliminates the double-buffer from the old `read_to_end` + parse approach.
fn read_dir_entries<R: io::Read>(reader: &mut R, count: usize) -> io::Result<Vec<DirEntry>> {
    let mut entries = Vec::with_capacity(count);
    let mut buf = [0u8; 24];
    for _ in 0..count {
        reader.read_exact(&mut buf)?;
        entries.push(DirEntry {
            tile_id: u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]),
            offset: u64::from_le_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]),
            length: u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
            run_length: u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]),
        });
    }
    Ok(entries)
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
    let mut z: u8 = 0;
    loop {
        z += 1;
        if z >= 31 {
            break;
        }
        let n = 1u64 << z;
        let next_base = (n * n * 4 - 1) / 3;
        if tile_id < next_base {
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
        // xy2d reflects within the full n x n grid (coords hold full values here),
        // unlike d2xy which reflects within the current sub-square size `s`.
        hilbert_rot(n, &mut x, &mut y, rx, ry);
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
