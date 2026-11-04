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
//! // Tile IDs must be strictly increasing in Hilbert order.
//! let gzipped_mvt = vec![0u8; 100]; // pre-compressed MVT data
//! writer.add_tile(0, 0, 0, &gzipped_mvt).expect("add tile");
//!
//! writer.write_to(std::path::Path::new("output.pmtiles")).expect("write");
//! ```

use rustc_hash::FxHashMap;
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufReader, BufWriter, Cursor, Seek, Write};
use std::path::{Path, PathBuf};

use crate::pmtiles_reader::{RawDirEntry, encode_directory};
use flate2::Compression;
use flate2::write::GzEncoder;

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

impl DedupStats {
    fn record_reuse(&mut self, length: u32, tile_count: u64) {
        self.tiles_reused += tile_count;
        self.bytes_saved += u64::from(length) * tile_count;
    }
}

#[derive(Clone, Copy)]
struct DedupKey {
    hash: u64,
    length: u32,
    fingerprint: u64,
}

/// One payload equality index shared by individual tiles and tile runs.
struct DedupIndex {
    buckets: FxHashMap<u64, Vec<(u64, u32, u64)>>,
    entries: usize,
    cap: usize,
}

impl DedupIndex {
    fn new(cap: usize) -> Self {
        Self {
            buckets: FxHashMap::default(),
            entries: 0,
            cap,
        }
    }

    fn lookup(&self, data: &[u8], stats: &mut DedupStats) -> io::Result<(DedupKey, Option<u64>)> {
        let length = u32::try_from(data.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "tile data exceeds 4 GB"))?;
        let (hash, fingerprint) = dedup_hashes(data);
        let key = DedupKey {
            hash,
            length,
            fingerprint,
        };

        if let Some(candidates) = self.buckets.get(&hash) {
            stats.candidates += 1;
            for &(offset, candidate_length, candidate_fingerprint) in candidates {
                if candidate_length != length {
                    stats.reject_len_mismatch += 1;
                } else if candidate_fingerprint != fingerprint {
                    stats.reject_fp_mismatch += 1;
                } else {
                    return Ok((key, Some(offset)));
                }
            }
        }

        Ok((key, None))
    }

    fn insert(&mut self, key: DedupKey, offset: u64, stats: &mut DedupStats) {
        if self.entries < self.cap {
            let bucket = self.buckets.entry(key.hash).or_default();
            if !bucket.is_empty() {
                stats.hash_bucket_collisions += 1;
            }
            bucket.push((offset, key.length, key.fingerprint));
            self.entries += 1;
        } else {
            stats.insert_skipped_cap += 1;
        }
    }

    fn entry_count(&self) -> usize {
        self.entries
    }

    fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    fn release(&mut self) {
        self.buckets = FxHashMap::default();
        self.entries = 0;
    }

    #[cfg(test)]
    fn inject(&mut self, hash: u64, offset: u64, length: u32, fingerprint: u64) {
        self.buckets
            .entry(hash)
            .or_default()
            .push((offset, length, fingerprint));
        self.entries += 1;
    }
}

fn dedup_hashes(data: &[u8]) -> (u64, u64) {
    let mut primary = DefaultHasher::new();
    data.hash(&mut primary);

    let mut fingerprint = DefaultHasher::new();
    DEDUP_FP2_SALT.hash(&mut fingerprint);
    data.hash(&mut fingerprint);

    (primary.finish(), fingerprint.finish())
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

const _: () = assert!(std::mem::size_of::<RawDirEntry>() == 24);

/// Storage contract for directory entries. Both stores expose the same
/// sequential read passes, so directory layout does not depend on storage.
trait DirSpill: Send + Sync {
    fn append(&mut self, entry: RawDirEntry) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
    fn entry_count(&self) -> u64;
    fn begin_read(&mut self) -> io::Result<()>;
    fn read_chunk(&mut self, count: usize) -> io::Result<Vec<RawDirEntry>>;
    fn release_read(&mut self);
    fn release_entries(&mut self);
    fn cleanup(&mut self);
}

/// Small in-memory directory store used by tests and small sinks.
struct MemoryDirSpill {
    entries: Vec<RawDirEntry>,
    read_position: usize,
}

impl MemoryDirSpill {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            read_position: 0,
        }
    }
}

impl DirSpill for MemoryDirSpill {
    fn append(&mut self, entry: RawDirEntry) -> io::Result<()> {
        self.entries.push(entry);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    #[allow(clippy::cast_possible_truncation)]
    fn entry_count(&self) -> u64 {
        self.entries.len() as u64
    }

    fn begin_read(&mut self) -> io::Result<()> {
        self.read_position = 0;
        Ok(())
    }

    fn read_chunk(&mut self, count: usize) -> io::Result<Vec<RawDirEntry>> {
        let end = self
            .read_position
            .checked_add(count)
            .filter(|&end| end <= self.entries.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "short directory store"))?;
        let chunk = self.entries[self.read_position..end].to_vec();
        self.read_position = end;
        Ok(chunk)
    }

    fn release_read(&mut self) {
        self.read_position = 0;
    }

    fn release_entries(&mut self) {
        self.entries = Vec::new();
        self.read_position = 0;
    }

    fn cleanup(&mut self) {}
}

/// Disk-backed directory spill. Its reader is reopened for each layout pass.
struct FileDirSpill {
    writer: BufWriter<File>,
    reader: Option<BufReader<File>>,
    path: PathBuf,
    count: u64,
}

impl FileDirSpill {
    fn new(tmp_dir: &Path) -> io::Result<Self> {
        let path = tmp_dir.join("dir_entries.bin");
        let file = File::create(&path)?;
        let writer = BufWriter::with_capacity(1 << 16, file);
        Ok(Self {
            writer,
            reader: None,
            path,
            count: 0,
        })
    }
}

impl DirSpill for FileDirSpill {
    fn append(&mut self, entry: RawDirEntry) -> io::Result<()> {
        self.writer.write_all(&entry.tile_id.to_le_bytes())?;
        self.writer.write_all(&entry.offset.to_le_bytes())?;
        self.writer.write_all(&entry.length.to_le_bytes())?;
        self.writer.write_all(&entry.run_length.to_le_bytes())?;
        self.count += 1;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    fn entry_count(&self) -> u64 {
        self.count
    }

    fn begin_read(&mut self) -> io::Result<()> {
        self.flush()?;
        self.reader = Some(BufReader::with_capacity(1 << 16, File::open(&self.path)?));
        Ok(())
    }

    fn read_chunk(&mut self, count: usize) -> io::Result<Vec<RawDirEntry>> {
        let reader = self.reader.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory read pass not started",
            )
        })?;
        read_dir_entries(reader, count)
    }

    fn release_read(&mut self) {
        self.reader = None;
    }

    fn release_entries(&mut self) {}

    fn cleanup(&mut self) {
        self.release_read();
        drop(std::fs::remove_file(&self.path));
    }
}

/// Concrete directory store keeps the writer's auto traits while dispatching
/// storage-specific operations through the spill contract.
enum DirectorySpill {
    Memory(MemoryDirSpill),
    File(FileDirSpill),
}

impl DirSpill for DirectorySpill {
    fn append(&mut self, entry: RawDirEntry) -> io::Result<()> {
        match self {
            Self::Memory(store) => store.append(entry),
            Self::File(store) => store.append(entry),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Memory(store) => store.flush(),
            Self::File(store) => store.flush(),
        }
    }

    fn entry_count(&self) -> u64 {
        match self {
            Self::Memory(store) => store.entry_count(),
            Self::File(store) => store.entry_count(),
        }
    }

    fn begin_read(&mut self) -> io::Result<()> {
        match self {
            Self::Memory(store) => store.begin_read(),
            Self::File(store) => store.begin_read(),
        }
    }

    fn read_chunk(&mut self, count: usize) -> io::Result<Vec<RawDirEntry>> {
        match self {
            Self::Memory(store) => store.read_chunk(count),
            Self::File(store) => store.read_chunk(count),
        }
    }

    fn release_read(&mut self) {
        match self {
            Self::Memory(store) => store.release_read(),
            Self::File(store) => store.release_read(),
        }
    }

    fn release_entries(&mut self) {
        match self {
            Self::Memory(store) => store.release_entries(),
            Self::File(store) => store.release_entries(),
        }
    }

    fn cleanup(&mut self) {
        match self {
            Self::Memory(store) => store.cleanup(),
            Self::File(store) => store.cleanup(),
        }
    }
}

/// A seekable archive sink shared by in-memory and streaming writers.
/// Streaming keeps tile data at its final offset in the partial archive;
/// the memory sink uses a Cursor for tests and small outputs.
enum BlobSink {
    Memory(Cursor<Vec<u8>>),
    File {
        writer: BufWriter<File>,
        partial_path: PathBuf,
    },
}

impl BlobSink {
    #[allow(clippy::cast_possible_truncation)]
    fn memory() -> Self {
        let mut cursor = Cursor::new(vec![0_u8; INIT_SECTION as usize]);
        cursor.set_position(INIT_SECTION);
        Self::Memory(cursor)
    }

    fn finish(&mut self, path: &Path) -> io::Result<()> {
        match self {
            Self::Memory(cursor) => {
                cursor.flush()?;
                let file = File::create(path)?;
                let mut writer = BufWriter::with_capacity(1 << 20, file);
                writer.write_all(cursor.get_ref())?;
                writer.flush()
            }
            Self::File {
                writer,
                partial_path,
            } => {
                writer.flush()?;
                std::fs::rename(partial_path.as_path(), path)
            }
        }
    }
}

impl Write for BlobSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Memory(cursor) => cursor.write(buf),
            Self::File { writer, .. } => writer.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Memory(cursor) => cursor.flush(),
            Self::File { writer, .. } => writer.flush(),
        }
    }
}

impl Seek for BlobSink {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        match self {
            Self::Memory(cursor) => cursor.seek(position),
            Self::File { writer, .. } => writer.seek(position),
        }
    }
}

/// Tile data and the trailing archive sections share one seekable sink.
struct TileBlob {
    sink: BlobSink,
    data_length: u64,
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Accumulates tiles and writes a PMTiles v3 archive.
///
/// Tile IDs must increase strictly in Hilbert order across calls to
/// [`add_tile()`](Self::add_tile) and [`add_run()`](Self::add_run).
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
    /// Seekable archive sink with tile data appended at the fixed data offset.
    blob: TileBlob,
    /// Total number of tiles addressed (including deduped references).
    num_addressed: u64,
    /// Current run being built (flushed when a new non-extending tile arrives).
    current_run: Option<RawDirEntry>,
    /// Directory entries (in-memory or streamed to disk).
    dir_store: DirectorySpill,
    /// Shared payload equality index for individual tiles and tile runs.
    dedup: DedupIndex,
    /// Number of unique tile contents (after dedup).
    unique_count: u64,
    /// Dedup behavior counters.
    dedup_stats: DedupStats,
    /// Last tile ID accepted, including the end of an accepted run.
    last_tile_id: Option<u64>,
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
            blob: TileBlob {
                sink: BlobSink::memory(),
                data_length: 0,
            },
            num_addressed: 0,
            current_run: None,
            dir_store: DirectorySpill::Memory(MemoryDirSpill::new()),
            dedup: DedupIndex::new(MAX_DEDUP_ENTRIES),
            unique_count: 0,
            dedup_stats: DedupStats::default(),
            last_tile_id: None,
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

        let dir_store = DirectorySpill::File(FileDirSpill::new(tmp_dir)?);

        Ok(PmtilesWriter {
            config,
            tile_data_format: TileDataFormat::Mvt,
            tile_data_compression: TileDataCompression::Gzip,
            source_pbf_filename: None,
            osmosis_replication_timestamp: None,
            metadata_extensions: Vec::new(),
            ocean_only_metadata: false,
            metadata_verbatim: None,
            blob: TileBlob {
                sink: BlobSink::File {
                    writer,
                    partial_path: blob_path,
                },
                data_length: 0,
            },
            num_addressed: 0,
            current_run: None,
            dir_store,
            dedup: DedupIndex::new(MAX_DEDUP_ENTRIES),
            unique_count: 0,
            dedup_stats: DedupStats::default(),
            last_tile_id: None,
        })
    }

    /// Add a tile. `data` must already be gzip-compressed.
    /// Tile IDs MUST be strictly increasing in Hilbert order.
    /// Returns `true` if unique, `false` if deduplicated.
    ///
    /// # Errors
    /// Returns `io::Error` if the coordinates are invalid, the tile ID is not
    /// increasing, the payload is too large, or a streaming file write fails.
    #[hotpath::measure]
    pub fn add_tile(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> io::Result<bool> {
        let tile_id = try_xy_to_tile_id(z, x, y)?;
        self.check_tile_order(tile_id)?;
        let (key, duplicate_offset) = self.dedup.lookup(data, &mut self.dedup_stats)?;

        if let Some(offset) = duplicate_offset {
            self.dedup_stats.record_reuse(key.length, 1);
            self.push_dir_entry(tile_id, offset, key.length)?;
            self.last_tile_id = Some(tile_id);
            return Ok(false);
        }

        let offset = self.blob.data_length;
        self.blob.sink.write_all(data)?;
        self.blob.data_length += u64::from(key.length);

        self.dedup.insert(key, offset, &mut self.dedup_stats);
        self.push_dir_entry(tile_id, offset, key.length)?;
        self.unique_count += 1;
        self.last_tile_id = Some(tile_id);
        Ok(true)
    }

    /// Add a consecutive run sharing one compressed payload. This deliberately
    /// preserves intra-run sharing even after the global dedup insertion cap.
    /// Tile IDs in this run and all previous calls must be strictly increasing.
    ///
    /// # Errors
    /// Returns `io::Error` for a zero or overflowing run, a non-increasing tile
    /// ID, a payload that is too large, or a streaming write failure.
    #[hotpath::measure]
    pub fn add_run(&mut self, tile_id: u64, run_length: u32, data: &[u8]) -> io::Result<bool> {
        if run_length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PMTiles run length must be non-zero",
            ));
        }
        let run_end = tile_id.checked_add(u64::from(run_length)).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "PMTiles tile ID run overflows")
        })?;
        self.check_tile_order(tile_id)?;
        let (key, duplicate_offset) = self.dedup.lookup(data, &mut self.dedup_stats)?;

        let (offset, stored) = if let Some(offset) = duplicate_offset {
            self.dedup_stats
                .record_reuse(key.length, u64::from(run_length));
            (offset, false)
        } else {
            let offset = self.blob.data_length;
            self.blob.sink.write_all(data)?;
            self.blob.data_length += u64::from(key.length);
            self.dedup.insert(key, offset, &mut self.dedup_stats);
            self.unique_count += 1;
            (offset, true)
        };
        self.push_dir_run(tile_id, run_length, offset, key.length)?;
        self.last_tile_id = Some(run_end - 1);
        if stored && run_length > 1 {
            self.dedup_stats
                .record_reuse(key.length, u64::from(run_length - 1));
        }
        Ok(stored)
    }

    fn check_tile_order(&self, tile_id: u64) -> io::Result<()> {
        if let Some(previous) = self.last_tile_id
            && tile_id <= previous
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "PMTiles tile IDs must be strictly increasing: got {tile_id} after {previous}"
                ),
            ));
        }
        Ok(())
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
        crate::debug::emit_counter_usize("pmtiles_dedup_entries", self.dedup.entry_count());
        let dedup_bytes_est = self.dedup.entry_count() * std::mem::size_of::<(u64, u32, u64)>()
            + self.dedup.bucket_count()
                * (std::mem::size_of::<u64>() + std::mem::size_of::<Vec<(u64, u32, u64)>>());
        crate::debug::emit_counter_usize("pmtiles_dedup_bytes_est", dedup_bytes_est);

        // Free dedup map - no longer needed after all tiles are added.
        self.dedup.release();

        // Build directories through sequential passes over the shared spill
        // interface. File-backed leaf construction reads bounded chunks.
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

        // Best-effort cleanup of the streaming directory spill.
        self.dir_store.cleanup();

        let data_length = self.blob.data_length;

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

        self.blob.sink.seek(io::SeekFrom::Start(metadata_offset))?;
        self.blob.sink.write_all(&metadata_compressed)?;
        self.blob.sink.write_all(&leaf_bytes)?;
        self.blob.sink.seek(io::SeekFrom::Start(0))?;
        self.blob.sink.write_all(&header)?;
        self.blob.sink.write_all(&root_bytes)?;
        self.blob.sink.flush()?;
        self.blob.sink.finish(path)?;
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
            self.dir_store.append(run)?;
        }
        Ok(())
    }

    /// Record a directory entry, extending the current run if possible.
    fn push_dir_entry(&mut self, tile_id: u64, offset: u64, length: u32) -> io::Result<()> {
        self.num_addressed += 1;
        if let Some(run) = self.current_run.as_mut()
            && run.tile_id.checked_add(u64::from(run.run_length)) == Some(tile_id)
            && offset == run.offset
            && length == run.length
            && let Some(merged_length) = run.run_length.checked_add(1)
        {
            run.run_length = merged_length;
            return Ok(());
        }
        // Flush old run if any, then start new one
        self.flush_run()?;
        self.current_run = Some(RawDirEntry {
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
        if let Some(run) = &mut self.current_run
            && run.tile_id.checked_add(u64::from(run.run_length)) == Some(tile_id)
            && offset == run.offset
            && length == run.length
            && let Some(merged_length) = run.run_length.checked_add(run_length)
        {
            self.num_addressed += u64::from(run_length);
            run.run_length = merged_length;
            return Ok(());
        }
        self.num_addressed += u64::from(run_length);
        self.flush_run()?;
        self.current_run = Some(RawDirEntry {
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
    fn collect_dir_entries(&mut self) -> io::Result<Vec<RawDirEntry>> {
        self.flush_run()?;
        self.dir_store.flush()?;
        let count = directory_entry_count(self.dir_store.entry_count())?;
        self.dir_store.begin_read()?;
        let entries = self.dir_store.read_chunk(count)?;
        self.dir_store.release_read();
        self.dir_store.release_entries();
        Ok(entries)
    }

    /// Override the dedup map capacity. Caps the payload dedup map's entry
    /// count; tile content is identical under any cap - what changes is how
    /// many duplicate payload copies the archive stores, and the map's RAM
    /// (~56 bytes/entry).
    pub fn set_dedup_cap(&mut self, cap: usize) {
        self.dedup.cap = cap;
    }

    /// Inject a dedup entry with specific hash/fingerprint values (test-only).
    /// Used to test fingerprint rejection without needing real hash collisions.
    #[cfg(test)]
    fn inject_dedup_entry(&mut self, hash1: u64, offset: u64, length: u32, fp2: u64) {
        self.dedup.inject(hash1, offset, length, fp2);
    }

    /// Compute the primary hash and secondary fingerprint for the given data (test-only).
    /// Returns (hash1, hash2).
    #[cfg(test)]
    fn compute_dedup_hashes(data: &[u8]) -> (u64, u64) {
        dedup_hashes(data)
    }

    /// Build root and leaf directory bytes from the dir store.
    ///
    /// Directory storage supplies sequential passes; one layout loop chooses
    /// root-only output or retries leaf fanout until the compressed root fits.
    /// Streaming stores reread their spill file per attempt, while the memory
    /// sink is intended for tests and small outputs.
    ///
    /// Returns `(root_compressed, leaf_compressed, num_entries)`.
    #[hotpath::measure]
    fn finalize_directories(&mut self) -> io::Result<(Vec<u8>, Vec<u8>, u64)> {
        self.flush_run()?;
        let result = layout_directories(&mut self.dir_store);
        self.dir_store.release_read();
        self.dir_store.release_entries();
        result
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

/// Convert the stored entry count for a platform-sized read pass.
fn directory_entry_count(count: u64) -> io::Result<usize> {
    usize::try_from(count).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "PMTiles directory entry count exceeds addressable memory",
        )
    })
}

/// Choose root-only output or retry larger leaf chunks until the compressed
/// root fits the reserved prologue. Every retry reads through the same spill
/// interface, so memory and file stores use identical layout decisions.
#[hotpath::measure]
fn layout_directories(store: &mut dyn DirSpill) -> io::Result<(Vec<u8>, Vec<u8>, u64)> {
    const MAX_ROOT_ENTRIES: usize = 16384;
    const LEAF_SIZE: usize = 4096;

    store.flush()?;
    let entry_count = store.entry_count();
    let count = directory_entry_count(entry_count)?;
    let mut root_only = count <= MAX_ROOT_ENTRIES;
    let mut leaf_size = LEAF_SIZE;

    loop {
        if root_only {
            store.begin_read()?;
            let entries = store.read_chunk(count)?;
            store.release_read();
            let root = gzip_compress(&encode_directory(&entries)?)?;
            if root.len() <= MAX_ROOT_BYTES {
                return Ok((root, Vec::new(), entry_count));
            }
            root_only = false;
        } else {
            let (root, leaves) = build_leaf_directories(store, count, leaf_size)?;
            if root.len() <= MAX_ROOT_BYTES {
                return Ok((root, leaves, entry_count));
            }
            leaf_size = leaf_size.checked_mul(2).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "PMTiles leaf size overflow")
            })?;
        }
    }
}

/// Build leaf directories from the current spill store in `leaf_size` chunks.
/// Returns (root_compressed, all_leaves_compressed).
#[hotpath::measure]
fn build_leaf_directories(
    store: &mut dyn DirSpill,
    count: usize,
    leaf_size: usize,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    store.begin_read()?;
    let mut leaf_blob: Vec<u8> = Vec::new();
    let mut root_entries: Vec<RawDirEntry> = Vec::new();
    let mut remaining = count;

    while remaining > 0 {
        let chunk_size = remaining.min(leaf_size);
        let chunk = store.read_chunk(chunk_size)?;
        remaining -= chunk_size;
        let first_tile_id = chunk[0].tile_id;

        let leaf_raw = encode_directory(&chunk)?;
        let compressed = gzip_compress(&leaf_raw)?;

        #[allow(clippy::cast_possible_truncation)]
        let leaf_len = compressed.len() as u32;
        let leaf_offset = leaf_blob.len() as u64;
        leaf_blob.extend_from_slice(&compressed);

        // run_length=0 marks a leaf directory pointer
        root_entries.push(RawDirEntry {
            tile_id: first_tile_id,
            offset: leaf_offset,
            length: leaf_len,
            run_length: 0,
        });
    }

    let root_raw = encode_directory(&root_entries)?;
    let root_compressed = gzip_compress(&root_raw)?;

    Ok((root_compressed, leaf_blob))
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
fn read_dir_entries<R: io::Read>(reader: &mut R, count: usize) -> io::Result<Vec<RawDirEntry>> {
    let mut entries = Vec::with_capacity(count);
    let mut buf = [0u8; 24];
    for _ in 0..count {
        reader.read_exact(&mut buf)?;
        entries.push(RawDirEntry {
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
///
/// Panics when the coordinates cannot be represented as a PMTiles tile ID.
/// Use [`try_xy_to_tile_id`] when coordinates are not already validated.
#[inline]
pub fn xy_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
    match try_xy_to_tile_id(z, x, y) {
        Ok(tile_id) => tile_id,
        Err(error) => panic!("{error}"),
    }
}

/// Convert validated tile coordinates to a PMTiles Hilbert tile ID.
///
/// PMTiles tile IDs fit in `u64` for zoom levels through 31. Coordinates must
/// satisfy `x < 2^z` and `y < 2^z`.
pub fn try_xy_to_tile_id(z: u8, x: u32, y: u32) -> io::Result<u64> {
    if z > 31 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PMTiles tile zoom must be at most 31",
        ));
    }
    let grid_size = 1_u32 << z;
    if x >= grid_size || y >= grid_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PMTiles tile coordinates are outside the zoom grid",
        ));
    }
    if z == 0 {
        return Ok(0);
    }
    let n = u64::from(grid_size);
    // Cumulative tiles for zoom levels 0..z-1: (4^z - 1) / 3
    let base = (n * n - 1) / 3;
    let d = hilbert_xy2d(grid_size, x, y);
    base.checked_add(d)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "PMTiles tile ID exceeds u64"))
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
