//! The one PMTiles v3 reader.
//!
//! Every consumer - verify, inspect, svg, diag, the ocean artifact, brokkr's
//! corpus gate - reads through [`Archive`], so opening, header decoding,
//! section bounds, directory traversal and metadata follow one set of rules.
//! Only the byte source differs: [`FileSource`] (positional reads into owned
//! buffers, the public default) or the crate-internal memory map the ocean
//! artifact uses to borrow tile blobs without copying.
//!
//! Opening decodes the header and nothing else, so a diagnostic can still
//! describe an archive whose sections or compression it cannot read. Every
//! later access is bounded on its own; [`Archive::check_layout`] is the
//! explicit whole-layout check for workflows that want to refuse up front.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Mutex;

use flate2::read::GzDecoder;

use super::directory::{BlobRef, TileRun, TileRuns, WireEntry, decode_directory};
use super::header::{Compression, HEADER_LEN, Header, Section, TileDecoder};
use crate::tiling::{ARCHIVE_ID_END, TileId};

// ---------------------------------------------------------------------------
// Gzip
// ---------------------------------------------------------------------------

// A DEFLATE length/distance pair emits at most 258 bytes and consumes at
// least two bits, giving a conservative maximum of 1032 output bytes per
// compressed input byte. Gzip framing only makes the ratio smaller.
const GZIP_MAX_EXPANSION_PER_INPUT_BYTE: usize = 1032;

/// Decompress one gzip member within the maximum expansion DEFLATE permits.
/// This bounds output by the input rather than by a dataset-size policy, so
/// large directory leaves and metadata stay readable.
pub(crate) fn gzip_decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    gzip_decompress_labeled(data, "gzip payload")
}

fn gzip_decompress_labeled(data: &[u8], label: &str) -> io::Result<Vec<u8>> {
    let max_output = data.len().saturating_mul(GZIP_MAX_EXPANSION_PER_INPUT_BYTE);
    let mut out = Vec::new();
    // Read one byte beyond the limit so oversized output is distinguishable
    // from output that ends exactly at the limit.
    let probe_limit = u64::try_from(max_output)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "gzip output limit too large"))?
        .saturating_add(1);
    GzDecoder::new(data)
        .take(probe_limit)
        .read_to_end(&mut out)
        .map_err(|e| io::Error::new(e.kind(), format!("{label}: {e}")))?;
    if out.len() > max_output {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("decompressed {label} exceeds output limit"),
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Byte sources
// ---------------------------------------------------------------------------

/// Where an archive's bytes come from. `len` is captured when the source is
/// opened; `bytes(offset, length)` returns exactly `length` bytes or an
/// error, and never reads past `len`. Neither source gives a snapshot: an
/// archive modified while it is read may yield inconsistent bytes. A file
/// truncated under a [`FileSource`] becomes an I/O error.
pub trait Source {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn bytes(&self, offset: u64, length: u64, label: &str) -> io::Result<Cow<'_, [u8]>>;
}

fn checked_range(offset: u64, length: u64, source_len: u64, label: &str) -> io::Result<usize> {
    let end = offset.checked_add(length).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} range overflow"),
        )
    })?;
    if end > source_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("{label} outside archive"),
        ));
    }
    usize::try_from(length).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} length does not fit address space"),
        )
    })
}

/// Positional reads into owned buffers. Safe for any file.
pub struct FileSource {
    file: Mutex<File>,
    len: u64,
}

impl Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn bytes(&self, offset: u64, length: u64, label: &str) -> io::Result<Cow<'_, [u8]>> {
        let size = checked_range(offset, length, self.len, label)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|e| io::Error::other(format!("cannot allocate {label} buffer: {e}")))?;
        bytes.resize(size, 0);
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("archive file lock poisoned"))?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
        Ok(Cow::Owned(bytes))
    }
}

/// A read-only memory map; reads borrow it and allocate nothing.
pub struct MapSource {
    map: memmap2::Mmap,
}

impl Source for MapSource {
    fn len(&self) -> u64 {
        self.map.len() as u64
    }

    fn bytes(&self, offset: u64, length: u64, label: &str) -> io::Result<Cow<'_, [u8]>> {
        let size = checked_range(offset, length, self.len(), label)?;
        // checked_range bounded offset + size by the map length, a usize.
        #[allow(clippy::cast_possible_truncation)]
        let start = offset as usize;
        Ok(Cow::Borrowed(&self.map[start..start + size]))
    }
}

// ---------------------------------------------------------------------------
// Archive
// ---------------------------------------------------------------------------

/// Directory levels a traversal follows, the root counting as the first.
/// The spec discourages nesting below one leaf level; deeper trees are
/// refused as an unsupported layout, not as malformed archives.
const MAX_DIRECTORY_LEVELS: u8 = 4;

/// A PMTiles v3 archive with a decoded header.
pub struct Archive<S: Source = FileSource> {
    source: S,
    header: Header,
}

impl Archive<FileSource> {
    /// Open an archive for reading. Decodes the header only.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Self::from_source(FileSource {
            file: Mutex::new(file),
            len,
        })
    }
}

impl Archive<MapSource> {
    /// Open an archive as a memory map.
    ///
    /// # Safety
    ///
    /// The file must not be modified or truncated by any process while the
    /// returned archive, or anything borrowed from it, is alive: a mapped
    /// page changing or vanishing under a borrowed slice is undefined
    /// behavior (or SIGBUS), which no bounds check prevents. Nothing enforces
    /// this; the caller asserts it. Replacing the path is not a modification:
    /// this crate's archive writer publishes by renaming a fresh file over
    /// it, which leaves the mapped inode intact.
    pub(crate) unsafe fn open_mapped(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        if file.metadata()?.len() < HEADER_LEN as u64 {
            return Err(short_header());
        }
        // SAFETY: forwarded to this function's caller, see above.
        let map = unsafe { memmap2::MmapOptions::new().map(&file)? };
        Self::from_source(MapSource { map })
    }
}

fn short_header() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "archive shorter than PMTiles header",
    )
}

fn unsupported_internal(compression: Compression) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("unsupported PMTiles internal compression: {compression}"),
    )
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl<S: Source> Archive<S> {
    fn from_source(source: S) -> io::Result<Self> {
        if source.len() < HEADER_LEN as u64 {
            return Err(short_header());
        }
        let bytes = source.bytes(0, HEADER_LEN as u64, "header")?;
        let mut header = [0_u8; HEADER_LEN];
        header.copy_from_slice(&bytes);
        let header = Header::decode(&header)?;
        Ok(Self { source, header })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Archive length in bytes, as captured at open.
    pub fn file_len(&self) -> u64 {
        self.source.len()
    }

    /// Whether MVT-parsing workflows can read this archive's tiles.
    pub fn tile_decoder(&self) -> io::Result<TileDecoder> {
        TileDecoder::for_contract(self.header.tile_type, self.header.tile_compression)
    }

    /// Refuse any section the header declares outside the archive.
    pub fn check_layout(&self) -> io::Result<()> {
        let len = self.file_len();
        for (name, section) in self.header.sections() {
            let Some(end) = section.end() else {
                return Err(invalid(format!(
                    "{name} section offset+length overflows u64"
                )));
            };
            if end > len {
                return Err(invalid(format!(
                    "{name} section exceeds file bounds: offset {} + length {} > file size {len}",
                    section.offset, section.length
                )));
            }
        }
        Ok(())
    }

    /// Bytes `[offset, offset + length)` relative to `section`, refused when
    /// they leave the section, not merely the file.
    fn in_section(
        &self,
        section: Section,
        offset: u64,
        length: u64,
        label: &str,
    ) -> io::Result<Cow<'_, [u8]>> {
        let inside = offset
            .checked_add(length)
            .is_some_and(|end| end <= section.length);
        if !inside {
            return Err(invalid(format!(
                "{label} at {offset}+{length} lies outside its {} byte section",
                section.length
            )));
        }
        let absolute = section
            .offset
            .checked_add(offset)
            .ok_or_else(|| invalid(format!("{label} offset overflow")))?;
        self.source.bytes(absolute, length, label)
    }

    /// Refuse an internal compression this reader cannot undo, before any
    /// section is read: an operation known to fail must not allocate first.
    fn require_internal_supported(&self) -> io::Result<()> {
        match self.header.internal_compression {
            Compression::None | Compression::Gzip => Ok(()),
            other => Err(unsupported_internal(other)),
        }
    }

    fn internal_decompress<'a>(
        &self,
        raw: Cow<'a, [u8]>,
        label: &str,
    ) -> io::Result<Cow<'a, [u8]>> {
        match self.header.internal_compression {
            Compression::None => Ok(raw),
            Compression::Gzip => gzip_decompress_labeled(&raw, label).map(Cow::Owned),
            other => Err(unsupported_internal(other)),
        }
    }

    /// The metadata document as UTF-8.
    pub fn metadata(&self) -> io::Result<String> {
        self.require_internal_supported()?;
        let section = self.header.metadata;
        let raw = self.in_section(section, 0, section.length, "metadata")?;
        let raw = self.internal_decompress(raw, "metadata")?.into_owned();
        String::from_utf8(raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Every tile run, sorted, from a validated directory tree.
    ///
    /// Each directory owns the routing interval its parent gives it: an
    /// entry's interval runs from its id to the next entry's id in the same
    /// directory (run or pointer), the last entry inheriting the parent's
    /// end. A leaf's first id must equal its pointer's id, and every run must
    /// end inside its interval - the predecessor lookup a PMTiles client
    /// performs relies on both. Blobs must lie inside the tile data section.
    pub fn read_all_runs(&self) -> io::Result<TileRuns> {
        self.require_internal_supported()?;
        let root = self.header.root_dir;
        let raw = self
            .source
            .bytes(root.offset, root.length, "root directory")?;
        let entries = decode_directory(&self.internal_decompress(raw, "root directory")?)?;
        let mut runs = Vec::new();
        self.collect_runs(&entries, 0, ARCHIVE_ID_END, 1, &mut runs)?;
        TileRuns::from_unsorted(runs)
    }

    fn collect_runs(
        &self,
        entries: &[WireEntry],
        lo: u64,
        hi: u64,
        level: u8,
        runs: &mut Vec<TileRun>,
    ) -> io::Result<()> {
        for (index, entry) in entries.iter().enumerate() {
            let upper = entries.get(index + 1).map_or(hi, |next| next.tile_id);
            if entry.tile_id < lo || entry.tile_id >= upper {
                return Err(invalid(format!(
                    "directory entry {} lies outside its routing interval [{lo}, {upper})",
                    entry.tile_id
                )));
            }
            let start = TileId::new(entry.tile_id)
                .ok_or_else(|| invalid("directory id beyond the last addressable tile"))?;
            let Some(run_length) = NonZeroU32::new(entry.run_length) else {
                self.descend(entry, upper, level, runs)?;
                continue;
            };
            let blob = BlobRef {
                offset: entry.offset,
                length: entry.length,
            };
            let run = TileRun::new(start, run_length, blob).ok_or_else(|| {
                invalid("tile run reaches past the last addressable PMTiles tile")
            })?;
            if run.end() > upper {
                return Err(invalid(format!(
                    "tile run {}..{} crosses its routing interval end {upper}",
                    entry.tile_id,
                    run.end()
                )));
            }
            self.check_blob(blob)?;
            runs.push(run);
        }
        Ok(())
    }

    fn descend(
        &self,
        pointer: &WireEntry,
        hi: u64,
        level: u8,
        runs: &mut Vec<TileRun>,
    ) -> io::Result<()> {
        if level >= MAX_DIRECTORY_LEVELS {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported layout: more than {MAX_DIRECTORY_LEVELS} directory levels"),
            ));
        }
        if pointer.length == 0 {
            return Err(invalid("zero-length leaf directory pointer"));
        }
        let raw = self.in_section(
            self.header.leaf_dirs,
            pointer.offset,
            u64::from(pointer.length),
            "leaf directory",
        )?;
        let entries = decode_directory(&self.internal_decompress(raw, "leaf directory")?)?;
        match entries.first() {
            Some(first) if first.tile_id == pointer.tile_id => {}
            Some(first) => {
                return Err(invalid(format!(
                    "leaf directory starts at {} but its pointer names {}",
                    first.tile_id, pointer.tile_id
                )));
            }
            None => return Err(invalid("empty leaf directory")),
        }
        self.collect_runs(&entries, pointer.tile_id, hi, level + 1, runs)
    }

    fn check_blob(&self, blob: BlobRef) -> io::Result<()> {
        let inside = blob
            .offset
            .checked_add(u64::from(blob.length))
            .is_some_and(|end| end <= self.header.tile_data.length);
        if inside {
            Ok(())
        } else {
            Err(invalid(format!(
                "tile payload at {}+{} lies outside the {} byte tile data section",
                blob.offset, blob.length, self.header.tile_data.length
            )))
        }
    }

    /// The stored payload bytes, still compressed.
    pub fn raw_blob(&self, blob: BlobRef) -> io::Result<Cow<'_, [u8]>> {
        self.in_section(
            self.header.tile_data,
            blob.offset,
            u64::from(blob.length),
            "tile payload",
        )
    }

    /// One tile payload, decompressed by the header's tile compression (none
    /// or gzip; anything else is an error).
    pub fn read_tile(&self, blob: BlobRef) -> io::Result<Vec<u8>> {
        let compression = self.header.tile_compression;
        if !matches!(compression, Compression::None | Compression::Gzip) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported tile compression: {compression}"),
            ));
        }
        let raw = self.raw_blob(blob)?;
        if compression == Compression::Gzip {
            gzip_decompress_labeled(&raw, "tile payload")
        } else {
            Ok(raw.into_owned())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
mod tests {
    use super::super::directory::encode_directory;
    use super::super::header::{Clustering, E7Bbox, E7Center, TileType};
    use super::*;
    use flate2::write::GzEncoder;
    use std::io::Write;

    #[test]
    fn gzip_decompression_enforces_the_deflate_bound() {
        let payload = b"bounded gzip payload";
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload).unwrap();
        let compressed = encoder.finish().unwrap();
        assert_eq!(gzip_decompress(&compressed).unwrap().as_slice(), payload);
    }

    /// Lay out a hand-built archive: header, root, leaves, data, all with
    /// internal compression none, so directory shapes a conforming writer
    /// never produces can be tested.
    fn archive_with(root: &[WireEntry], leaves: &[u8], data_len: u64) -> Vec<u8> {
        let root_bytes = encode_directory(root).unwrap();
        let root_offset = HEADER_LEN as u64;
        let leaf_offset = root_offset + root_bytes.len() as u64;
        let data_offset = leaf_offset + leaves.len() as u64;
        let header = Header {
            root_dir: Section {
                offset: root_offset,
                length: root_bytes.len() as u64,
            },
            metadata: Section {
                offset: data_offset + data_len,
                length: 0,
            },
            leaf_dirs: Section {
                offset: leaf_offset,
                length: leaves.len() as u64,
            },
            tile_data: Section {
                offset: data_offset,
                length: data_len,
            },
            addressed_tiles: 0,
            tile_entries: 0,
            tile_contents: 0,
            clustering: Clustering::Clustered,
            internal_compression: Compression::None,
            tile_compression: Compression::None,
            tile_type: TileType::Mvt,
            min_zoom: 0,
            max_zoom: 14,
            bounds: E7Bbox::default(),
            center: E7Center::default(),
        };
        let mut bytes = header.encode().to_vec();
        bytes.extend_from_slice(&root_bytes);
        bytes.extend_from_slice(leaves);
        bytes.resize(bytes.len() + data_len as usize, 0);
        bytes
    }

    fn entry(tile_id: u64, offset: u64, length: u32, run_length: u32) -> WireEntry {
        WireEntry {
            tile_id,
            offset,
            length,
            run_length,
        }
    }

    fn runs_of(bytes: &[u8]) -> io::Result<TileRuns> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.pmtiles");
        std::fs::write(&path, bytes)?;
        Archive::open(&path)?.read_all_runs()
    }

    #[test]
    fn leaf_routing_is_validated_before_flattening() {
        let leaf = encode_directory(&[entry(10, 0, 4, 2), entry(14, 4, 4, 1)]).unwrap();
        let len = leaf.len() as u32;
        // Valid: the leaf interval [10, 20) holds runs 10..12 and 14..15.
        let ok = archive_with(&[entry(10, 0, len, 0), entry(20, 8, 4, 1)], &leaf, 12);
        assert_eq!(runs_of(&ok).unwrap().len(), 3);
        // The next root entry starts inside the leaf's last run.
        let crossing = archive_with(&[entry(10, 0, len, 0), entry(14, 8, 4, 1)], &leaf, 12);
        assert!(runs_of(&crossing).is_err());
        // Pointer id differs from the leaf's first id.
        let misnamed = archive_with(&[entry(9, 0, len, 0)], &leaf, 12);
        assert!(runs_of(&misnamed).is_err());
        // Pointer reaches past the leaf section.
        let outside = archive_with(&[entry(10, 1, len, 0)], &leaf, 12);
        assert!(runs_of(&outside).is_err());
        // Zero-length pointer.
        let empty = archive_with(&[entry(10, 0, 0, 0)], &leaf, 12);
        assert!(runs_of(&empty).is_err());
    }

    #[test]
    fn blobs_must_lie_inside_the_tile_data_section() {
        let ok = archive_with(&[entry(0, 0, 4, 1)], &[], 4);
        assert_eq!(runs_of(&ok).unwrap().len(), 1);
        let outside = archive_with(&[entry(0, 1, 4, 1)], &[], 4);
        assert!(runs_of(&outside).is_err());
    }

    #[test]
    fn nesting_is_followed_up_to_the_level_cap() {
        // A chain of leaves, each pointing at the next, ending in one run.
        let build = |levels: usize| {
            let mut leaves = Vec::new();
            let mut pointer = entry(5, 0, 0, 0);
            let mut chain = vec![encode_directory(&[entry(5, 0, 4, 1)]).unwrap()];
            for _ in 1..levels - 1 {
                let below = chain.last().unwrap();
                let offset: usize = chain.iter().map(Vec::len).sum::<usize>() - below.len();
                chain.push(
                    encode_directory(&[entry(5, offset as u64, below.len() as u32, 0)]).unwrap(),
                );
            }
            for dir in &chain {
                leaves.extend_from_slice(dir);
            }
            let top = chain.last().unwrap();
            pointer.offset = (leaves.len() - top.len()) as u64;
            pointer.length = top.len() as u32;
            archive_with(&[pointer], &leaves, 4)
        };
        assert_eq!(
            runs_of(&build(MAX_DIRECTORY_LEVELS as usize))
                .unwrap()
                .len(),
            1
        );
        let err = runs_of(&build(MAX_DIRECTORY_LEVELS as usize + 1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn open_reads_the_header_of_an_archive_it_cannot_otherwise_read() {
        let mut bytes = archive_with(&[entry(0, 0, 4, 1)], &[], 4);
        bytes[97] = 9; // internal compression outside the spec
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.pmtiles");
        std::fs::write(&path, &bytes).unwrap();
        let archive = Archive::open(&path).unwrap();
        assert_eq!(archive.header().internal_compression, Compression::Other(9));
        assert_eq!(
            archive.read_all_runs().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        std::fs::write(&path, b"PMTiles").unwrap();
        assert!(Archive::open(&path).is_err());
        std::fs::write(&path, b"").unwrap();
        assert!(Archive::open(&path).is_err());
    }
}
