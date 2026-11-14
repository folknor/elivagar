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
//! use elivagar::archive::{LonLatBbox, MapCenter, PmtilesConfig, PmtilesWriter};
//!
//! let config = PmtilesConfig::new(
//!     0,
//!     14,
//!     LonLatBbox { west: 8.0, south: 54.5, east: 15.2, north: 57.8 },
//!     MapCenter { lon: 11.5, lat: 56.0, zoom: 7 },
//! )
//! .expect("valid geography");
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

use super::directory::{WireEntry, encode_directory};
use super::header::{
    Clustering, Compression as HeaderCompression, E7Bbox, E7Center, HEADER_LEN, Header, LonLatBbox,
    MapCenter, Section, TileType,
};
use crate::tiling::TileId;
use flate2::Compression;
use flate2::write::GzEncoder;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// PMTiles writer configuration: the archive's zoom range and its header
/// geography. Built only through [`PmtilesConfig::new`], which validates the
/// geography, so a writer never discovers bad coordinates after the tile
/// work is done.
#[derive(Clone, Copy, Debug)]
pub struct PmtilesConfig {
    min_zoom: u8,
    max_zoom: u8,
    bounds: E7Bbox,
    center: E7Center,
}

impl PmtilesConfig {
    /// # Errors
    /// Refuses `min_zoom > max_zoom` and any bounds or center
    /// [`LonLatBbox::to_e7`] / [`MapCenter::to_e7`] refuse.
    pub fn new(
        min_zoom: u8,
        max_zoom: u8,
        bounds: LonLatBbox,
        center: MapCenter,
    ) -> io::Result<Self> {
        if min_zoom > max_zoom {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("PMTiles min zoom {min_zoom} exceeds max zoom {max_zoom}"),
            ));
        }
        Ok(Self {
            min_zoom,
            max_zoom,
            bounds: bounds.to_e7()?,
            center: center.to_e7()?,
        })
    }

    pub fn min_zoom(&self) -> u8 {
        self.min_zoom
    }

    pub fn max_zoom(&self) -> u8 {
        self.max_zoom
    }
}

/// Tile payload format stored in PMTiles tile data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileDataFormat {
    Mvt,
    Mlt,
}

impl TileDataFormat {
    pub(crate) fn header_tile_type(self) -> TileType {
        match self {
            Self::Mvt => TileType::Mvt,
            // Unknown in the header; the metadata carries the mlt marker.
            Self::Mlt => TileType::Unknown,
        }
    }

    fn metadata_format(self) -> &'static str {
        match self {
            Self::Mvt => "pbf",
            Self::Mlt => "mlt",
        }
    }

    pub(crate) fn metadata_payload(self) -> &'static str {
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
    pub(crate) fn header_compression(self) -> HeaderCompression {
        match self {
            Self::None => HeaderCompression::None,
            Self::Gzip => HeaderCompression::Gzip,
            Self::Brotli => HeaderCompression::Brotli,
            Self::Zstd => HeaderCompression::Zstd,
        }
    }

    pub(crate) fn metadata_value(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Gzip => "gzip",
            Self::Brotli => "brotli",
            Self::Zstd => "zstd",
        }
    }
}

/// Default payload dedup map entry cap used by a new writer. At planet scale,
/// unlimited dedup grows to ~7 GB. Capping at 1M entries keeps the map under
/// ~60 MB while still deduplicating the ocean fill tiles (which are added
/// early and remain cached).
pub const DEFAULT_DEDUP_CAP: usize = 1_000_000;

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
const MAX_ROOT_BYTES: usize = INIT_SECTION as usize - HEADER_LEN;

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
/// Buckets hold `(offset, length, fingerprint)` rather than a `BlobRef` plus
/// fingerprint: the tuple is this index's private 24-byte entry, which the
/// RAM-ledger counter in `write_to` sizes, and naming it would buy nothing.
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

const _: () = assert!(std::mem::size_of::<WireEntry>() == 24);

/// Storage contract for directory entries. Both stores expose the same
/// sequential read passes, so directory layout does not depend on storage.
trait DirSpill: Send + Sync {
    fn append(&mut self, entry: WireEntry) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
    fn entry_count(&self) -> u64;
    fn begin_read(&mut self) -> io::Result<()>;
    fn read_chunk(&mut self, count: usize) -> io::Result<Vec<WireEntry>>;
    fn release_read(&mut self);
    fn release_entries(&mut self);
    fn cleanup(&mut self);
}

/// Small in-memory directory store used by tests and small sinks.
struct MemoryDirSpill {
    entries: Vec<WireEntry>,
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
    fn append(&mut self, entry: WireEntry) -> io::Result<()> {
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

    fn read_chunk(&mut self, count: usize) -> io::Result<Vec<WireEntry>> {
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
    fn append(&mut self, entry: WireEntry) -> io::Result<()> {
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

    fn read_chunk(&mut self, count: usize) -> io::Result<Vec<WireEntry>> {
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
    fn append(&mut self, entry: WireEntry) -> io::Result<()> {
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

    fn read_chunk(&mut self, count: usize) -> io::Result<Vec<WireEntry>> {
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

/// A fresh sibling file an archive is built in before it is published by
/// rename. Opened with `create_new` under a per-process unique name, so it
/// can never alias an existing inode - not a stale partial, not a hard link
/// or symlink planted at a predictable name, not another writer's partial.
/// Removed on drop unless published (best effort), so a failed or abandoned
/// write does not normally leave it behind.
struct Partial {
    path: PathBuf,
    published: bool,
}

impl Partial {
    fn create(output_path: &Path) -> io::Result<(Self, File)> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        // A name left by a crashed process with a reused pid is skipped, not
        // fatal; `create_new` is what guarantees no existing file is opened.
        loop {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut name = output_path.as_os_str().to_owned();
            name.push(format!(".partial-{}-{n}", std::process::id()));
            let path = PathBuf::from(name);
            match File::options().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok((
                        Self {
                            path,
                            published: false,
                        },
                        file,
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Rename the (already closed) partial over `path`.
    fn publish(mut self, path: &Path) -> io::Result<()> {
        std::fs::rename(&self.path, path)?;
        self.published = true;
        Ok(())
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if !self.published {
            drop(std::fs::remove_file(&self.path));
        }
    }
}

/// A seekable archive sink shared by in-memory and streaming writers.
/// Streaming keeps tile data at its final offset in the partial archive;
/// the memory sink uses a Cursor for tests and small outputs.
///
/// Both modes publish by renaming a fresh sibling over the output path and
/// close every writable handle first, so no archive this crate writes ever
/// truncates or rewrites an existing inode. That is what keeps a reader
/// that memory-maps a previous archive at the same path (the ocean
/// artifact, `ocean/artifact.rs`) sound against this crate's archive writes.
enum BlobSink {
    Memory(Cursor<Vec<u8>>),
    File {
        writer: BufWriter<File>,
        partial: Partial,
    },
}

impl BlobSink {
    #[allow(clippy::cast_possible_truncation)]
    fn memory() -> Self {
        let mut cursor = Cursor::new(vec![0_u8; INIT_SECTION as usize]);
        cursor.set_position(INIT_SECTION);
        Self::Memory(cursor)
    }

    fn finish(self, path: &Path) -> io::Result<()> {
        match self {
            Self::Memory(cursor) => {
                let (partial, file) = Partial::create(path)?;
                let mut writer = BufWriter::with_capacity(1 << 20, file);
                writer.write_all(cursor.get_ref())?;
                drop(
                    writer
                        .into_inner()
                        .map_err(io::IntoInnerError::into_error)?,
                );
                partial.publish(path)
            }
            Self::File { writer, partial } => {
                drop(
                    writer
                        .into_inner()
                        .map_err(io::IntoInnerError::into_error)?,
                );
                partial.publish(path)
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
    metadata_extensions: MetadataExtensions,
    /// The producer's description of the tileset (name, type, declared
    /// layers). The writer knows no schema; without one the document
    /// declares no layers and carries no name or type.
    tileset: Option<Tileset>,
    /// Exact metadata supplied by an archive-preserving rewrite.  This is
    /// intentionally narrow: normal production writes always compose metadata.
    metadata_verbatim: Option<String>,
    /// Seekable archive sink with tile data appended at the fixed data offset.
    blob: TileBlob,
    /// Total number of tiles addressed (including deduped references).
    num_addressed: u64,
    /// Current run being built (flushed when a new non-extending tile arrives).
    current_run: Option<WireEntry>,
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

    /// Add the durable ocean artifact identity to archive metadata.
    pub(crate) fn set_ocean_artifact_metadata(
        &mut self,
        key: crate::ocean::OceanArtifactKey,
    ) -> io::Result<()> {
        if self.metadata_extensions.ocean_artifact.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "archive metadata already has ocean_artifact",
            ));
        }
        self.metadata_extensions.ocean_artifact = Some(key);
        Ok(())
    }

    /// Add the run's provenance member to archive metadata.
    /// Provenance owns this member's schema; the writer only composes it.
    pub(crate) fn set_elivagar_metadata(&mut self, value: serde_json::Value) -> io::Result<()> {
        if !value.is_object() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "elivagar metadata must be an object",
            ));
        }
        if self.metadata_extensions.elivagar.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "archive metadata already has elivagar",
            ));
        }
        self.metadata_extensions.elivagar = Some(value);
        Ok(())
    }

    /// Describe the tileset: its `name`, `type` and declared `vector_layers`.
    ///
    /// The schema is the producer's; the writer only intersects each declared
    /// layer's zoom range with the archive's and drops layers that miss it.
    pub fn set_tileset(&mut self, tileset: Tileset) {
        self.tileset = Some(tileset);
    }

    /// Preserve an existing metadata document during an archive rewrite.
    ///
    /// The corpus mutation instrument uses this so its output remains under
    /// exactly the source archive's provenance contract. This selects the
    /// verbatim document and ignores the writer's structured metadata fields.
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
            metadata_extensions: MetadataExtensions::default(),
            tileset: None,
            metadata_verbatim: None,
            blob: TileBlob {
                sink: BlobSink::memory(),
                data_length: 0,
            },
            num_addressed: 0,
            current_run: None,
            dir_store: DirectorySpill::Memory(MemoryDirSpill::new()),
            dedup: DedupIndex::new(DEFAULT_DEDUP_CAP),
            unique_count: 0,
            dedup_stats: DedupStats::default(),
            last_tile_id: None,
        }
    }

    /// Create a streaming writer. Tile data is written directly into a fresh
    /// `<output_path>.partial-<pid>-<n>` sibling at its final archive offset;
    /// `write_to()` completes the prologue and renames it to `output_path`. Directory
    /// entries stream to a temp file in `tmp_dir` as before.
    ///
    /// # Errors
    /// Returns `io::Error` if creating either file fails.
    pub fn new_streaming(
        config: PmtilesConfig,
        tmp_dir: &Path,
        output_path: &Path,
    ) -> io::Result<Self> {
        let (partial, file) = Partial::create(output_path)?;
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
            metadata_extensions: MetadataExtensions::default(),
            tileset: None,
            metadata_verbatim: None,
            blob: TileBlob {
                sink: BlobSink::File { writer, partial },
                data_length: 0,
            },
            num_addressed: 0,
            current_run: None,
            dir_store,
            dedup: DedupIndex::new(DEFAULT_DEDUP_CAP),
            unique_count: 0,
            dedup_stats: DedupStats::default(),
            last_tile_id: None,
        })
    }

    /// Add a tile. `data` must already match the configured format and compression.
    /// Tile IDs MUST be strictly increasing in Hilbert order.
    /// Returns `true` if unique, `false` if deduplicated.
    ///
    /// # Errors
    /// Returns `io::Error` if the coordinates are invalid, the tile ID is not
    /// increasing, the payload is too large, or a streaming file write fails.
    #[hotpath::measure]
    pub fn add_tile(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> io::Result<bool> {
        let tile_id = TileId::from_coord(z, x, y)?.raw();
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
    /// Returns `io::Error` for a zero run, a run reaching past the last
    /// addressable tile id, a non-increasing tile ID, a payload that is too
    /// large, or a streaming write failure.
    #[hotpath::measure]
    pub fn add_run(&mut self, tile_id: TileId, run_length: u32, data: &[u8]) -> io::Result<bool> {
        if run_length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PMTiles run length must be non-zero",
            ));
        }
        let tile_id = tile_id.raw();
        let run_end = tile_id
            .checked_add(u64::from(run_length))
            .filter(|&end| end <= crate::tiling::ARCHIVE_ID_END)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "PMTiles tile ID run reaches past the last addressable tile",
                )
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
    /// In-memory mode writes the whole archive to a fresh sibling of `path`
    /// in one pass. Streaming mode appends metadata and leaf directories
    /// after the already-in-place tile data and patches the header and root
    /// into the reserved init section of its partial. Both then close the
    /// file and rename it over `path` - so the archive appears at `path`
    /// only when complete, an existing file at `path` is replaced by a new
    /// inode rather than rewritten (a reader mapping the old one keeps valid
    /// pages), and in streaming mode `path` must be on the same filesystem
    /// as the `output_path` given to [`new_streaming`](Self::new_streaming)
    /// (they are the same path in every production caller). Consumes the
    /// writer, so a published archive can never be written through again.
    ///
    /// # Errors
    /// Returns `io::Error` if file creation, directory encoding, writing, or
    /// the final rename fails.
    #[hotpath::measure]
    pub fn write_to(mut self, path: &Path) -> io::Result<()> {
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
        let metadata_json = if let Some(metadata) = &self.metadata_verbatim {
            metadata.as_bytes().to_vec()
        } else {
            build_metadata(
                &self.config,
                self.tile_data_format,
                self.tile_data_compression,
                self.source_pbf_filename.as_deref(),
                self.osmosis_replication_timestamp,
                &self.metadata_extensions,
                self.tileset.as_ref(),
            )?
        };
        let metadata_compressed = gzip_compress(&metadata_json)?;

        // Best-effort cleanup of the streaming directory spill.
        self.dir_store.cleanup();

        let data_length = self.blob.data_length;

        // Layout: [header 127] [root_dir] [zeros to INIT_SECTION] [tile_data]
        // [metadata] [leaf_dirs]. finalize_directories() guarantees the root
        // fits the init section; the assert is the layout's load-bearing
        // invariant, not a recoverable condition.
        let root_dir_offset = HEADER_LEN as u64;
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

        let header = Header {
            root_dir: Section {
                offset: root_dir_offset,
                length: root_dir_length,
            },
            metadata: Section {
                offset: metadata_offset,
                length: metadata_length,
            },
            leaf_dirs: Section {
                offset: leaf_dirs_offset,
                length: leaf_dirs_length,
            },
            tile_data: Section {
                offset: data_offset,
                length: data_length,
            },
            addressed_tiles: self.num_addressed,
            tile_entries: num_entries,
            tile_contents: self.unique_count,
            clustering: Clustering::Clustered,
            internal_compression: HeaderCompression::Gzip,
            tile_compression: self.tile_data_compression.header_compression(),
            tile_type: self.tile_data_format.header_tile_type(),
            min_zoom: self.config.min_zoom,
            max_zoom: self.config.max_zoom,
            bounds: self.config.bounds,
            center: self.config.center,
        }
        .encode();

        self.blob.sink.seek(io::SeekFrom::Start(metadata_offset))?;
        self.blob.sink.write_all(&metadata_compressed)?;
        self.blob.sink.write_all(&leaf_bytes)?;
        self.blob.sink.seek(io::SeekFrom::Start(0))?;
        self.blob.sink.write_all(&header)?;
        self.blob.sink.write_all(&root_bytes)?;
        self.blob.sink.flush()?;
        self.blob.sink.finish(path)
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
        self.current_run = Some(WireEntry {
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
        self.current_run = Some(WireEntry {
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
    fn collect_dir_entries(&mut self) -> io::Result<Vec<WireEntry>> {
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
    /// (~56 bytes/entry). A cap of zero prevents new map entries. Existing
    /// entries remain eligible for lookups, so set zero before adding tiles to
    /// disable tile-to-tile deduplication from the start. Run entries still
    /// share one payload across their addressed tile range.
    ///
    /// The cap stays a plain count on purpose. Zero is not a sentinel here:
    /// it is a capacity of zero entries, handled by the same insert-if-below
    /// arithmetic as any other cap, so an `Off`/`Cap(n)` policy enum would
    /// describe a mode the writer does not have. Only the pipeline's
    /// `TilegenConfig::dedup_cap` is `Option<NonZeroUsize>`, because there
    /// `None` means "the default".
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
    let mut root_entries: Vec<WireEntry> = Vec::new();
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
        root_entries.push(WireEntry {
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

#[derive(Default)]
struct MetadataExtensions {
    ocean_artifact: Option<crate::ocean::OceanArtifactKey>,
    elivagar: Option<serde_json::Value>,
}

/// The producer's description of a tileset, written into the metadata
/// document's `name`, `type` and `vector_layers` members.
///
/// The container writer knows no tile schema: which layers exist, what the
/// tileset is called and whether it is a base layer are the producer's
/// policy, handed in through [`PmtilesWriter::set_tileset`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tileset {
    pub name: String,
    /// The TileJSON `type` member, e.g. `baselayer` or `overlay`.
    pub layer_type: String,
    /// Declared layers in document order.
    pub vector_layers: Vec<VectorLayer>,
}

/// One declared layer: its id and the lowest zoom it appears at. The
/// advertised range is this intersected with the archive's zoom range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorLayer {
    pub id: String,
    pub min_zoom: u8,
    /// TileJSON field names and types (String, Number, Boolean).
    pub fields: std::collections::BTreeMap<String, String>,
}

/// The archive's JSON schema and serialization live together here, inside
/// the writer. The schema-specific inputs (tileset name, type and layer set,
/// the ocean artifact key, the provenance member) are producer policy handed
/// in through the writer's setters; the writer contributes only container
/// facts (payload format and compression, the archive zoom range and the
/// clamp of each layer to it). A separate `archive::metadata` module was
/// weighed and refused, since moving the document alone would add a hop
/// without moving ownership. The read side stays `serde_json::Value`. Field
/// declaration order preserves the existing wire order, and the two optional
/// extension keys are distinct fields, so the composed document cannot emit
/// either key twice.
mod metadata {
    use super::{MetadataExtensions, PmtilesConfig, TileDataCompression, TileDataFormat, Tileset};
    use serde::Serialize;
    use std::io;

    #[derive(Serialize)]
    struct MetadataDocument<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<&'a str>,
        format: &'static str,
        tile_payload_format: &'static str,
        tile_compression: &'static str,
        #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
        layer_type: Option<&'a str>,
        minzoom: u8,
        maxzoom: u8,
        vector_layers: Vec<VectorLayer<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source_pbf: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        osmosis_replication_timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ocean_artifact: Option<&'a crate::ocean::OceanArtifactKey>,
        #[serde(skip_serializing_if = "Option::is_none")]
        elivagar: Option<&'a serde_json::Value>,
    }

    #[derive(Serialize)]
    struct VectorLayer<'a> {
        id: &'a str,
        fields: &'a std::collections::BTreeMap<String, String>,
        minzoom: u8,
        maxzoom: u8,
    }

    pub(super) fn build(
        config: &PmtilesConfig,
        tile_data_format: TileDataFormat,
        tile_data_compression: TileDataCompression,
        source_pbf_filename: Option<&str>,
        osmosis_replication_timestamp: Option<i64>,
        extensions: &MetadataExtensions,
        tileset: Option<&Tileset>,
    ) -> io::Result<Vec<u8>> {
        let document = MetadataDocument {
            name: tileset.map(|t| t.name.as_str()),
            format: tile_data_format.metadata_format(),
            tile_payload_format: tile_data_format.metadata_payload(),
            tile_compression: tile_data_compression.metadata_value(),
            layer_type: tileset.map(|t| t.layer_type.as_str()),
            minzoom: config.min_zoom,
            maxzoom: config.max_zoom,
            // Each layer advertises its own zoom range intersected with the
            // archive's; a layer whose range misses the archive's entirely
            // has no tiles and is not advertised.
            vector_layers: tileset
                .map(|t| t.vector_layers.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|layer| VectorLayer {
                    id: &layer.id,
                    fields: &layer.fields,
                    minzoom: layer.min_zoom.max(config.min_zoom),
                    maxzoom: config.max_zoom,
                })
                .filter(|layer| layer.minzoom <= layer.maxzoom)
                .collect(),
            source_pbf: source_pbf_filename,
            osmosis_replication_timestamp,
            ocean_artifact: extensions.ocean_artifact.as_ref(),
            elivagar: extensions.elivagar.as_ref(),
        };
        serde_json::to_vec(&document).map_err(io::Error::other)
    }
}

fn build_metadata(
    config: &PmtilesConfig,
    tile_data_format: TileDataFormat,
    tile_data_compression: TileDataCompression,
    source_pbf_filename: Option<&str>,
    osmosis_replication_timestamp: Option<i64>,
    metadata_extensions: &MetadataExtensions,
    tileset: Option<&Tileset>,
) -> io::Result<Vec<u8>> {
    metadata::build(
        config,
        tile_data_format,
        tile_data_compression,
        source_pbf_filename,
        osmosis_replication_timestamp,
        metadata_extensions,
        tileset,
    )
}

// ---------------------------------------------------------------------------
// Binary helpers
// ---------------------------------------------------------------------------

/// Read `count` directory entries from a reader, 24 bytes at a time.
/// Eliminates the double-buffer from the old `read_to_end` + parse approach.
fn read_dir_entries<R: io::Read>(reader: &mut R, count: usize) -> io::Result<Vec<WireEntry>> {
    let mut entries = Vec::with_capacity(count);
    let mut buf = [0u8; 24];
    for _ in 0..count {
        reader.read_exact(&mut buf)?;
        entries.push(WireEntry {
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

// ---------------------------------------------------------------------------
// Tests (see writer_tests.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "writer_tests.rs"]
mod tests;
