//! The linked-elivagar-crate surface used by the gate's archive consumers.
//!
//! The gate does not decode PMTiles itself: it links the elivagar library
//! and reads archives in-process through its reader/decoder/writer. This
//! module is the seam - the one place that names the elivagar API the gate
//! depends on, so `corpus/`, `regress/` and `compare_tiles` import from here
//! rather than reaching into `elivagar::` directly. Durable archive paths
//! are not part of this seam; they live in `elivagar_devkit::archive_store`. (Ported from
//! brokkr, where this code ran as `brokkr pmtiles-corpus`; "brokkr" below
//! means this gate.)
//!
//! The canonical tile hash is NOT here: it is brokkr's (`corpus/canonical.rs`),
//! built over the `decode_detail_attr` + `protohoggr` primitives this seam
//! re-exports.
//!
//! Tile ids cross the seam as raw `u64`: brokkr's digest, diff and regress
//! layers do run arithmetic on them, and the end of a run is not an address.
//! elivagar's `TileId` stays at the boundary, where `xy_to_tile_id`,
//! `checked_tile_id` and `tile_id_to_zxy` convert.

use std::borrow::Cow;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;

use elivagar::archive::Archive;
use elivagar::tiling::zoom_ids;
use elivagar::{DecodeError, DecodeErrorKind};
// Reader surface.
pub use elivagar::archive::{BlobRef, Header};
// Writer surface (mutate and the fixtures) + PMTiles addressing.
pub use elivagar::archive::header::e7_to_degrees;
pub use elivagar::archive::{LonLatBbox, MapCenter, PmtilesConfig, PmtilesWriter};
pub use elivagar::tiling::{ARCHIVE_MAX_ZOOM, TileId};
// Decoder surface. DetailAttr/CanonRingRole/Strictness + decode_detail_attr feed
// the streaming canonical hash; the full wire-order DetailTile decoder feeds the
// detail canonical form (regress's "same tile", pinned against streaming by the
// equivalence tests).
pub use elivagar::tile_detail::{
    CanonRingRole, DetailAttr, DetailComponent, DetailFeature, DetailLayer, DetailRing, DetailTile,
    Strictness,
};
// MVT cursor and encoder primitives enter brokkr through this adapter as well,
// so corpus code never depends on protohoggr directly.
pub use protohoggr::{WIRE_32BIT, WIRE_64BIT, WIRE_LEN, WIRE_VARINT, encode_varint};
// The tile encoders serve the fixtures only.
#[cfg(test)]
pub use protohoggr::{encode_bytes_field_always, encode_tag, encode_varint_field_always};

/// Convert a panic in the linked archive reader into the same error channel as
/// malformed archive data. The linked PMTiles directory decoder is fallible
/// for ordinary corruption, but its parser also contains panic-capable paths.
fn linked_io<T>(operation: &'static str, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("linked elivagar {operation} panicked"),
        )),
    }
}

/// Carry an elivagar decode failure in brokkr's `io::Error` channel, keeping
/// its class where consumers branch on `io::ErrorKind`: malformed bytes are
/// `InvalidData`, an unsupported layout or codec `Unsupported`, and an OS
/// failure keeps its original kind (`NotFound` for a missing archive). The
/// typed error stays reachable as the source.
fn decode_io(error: DecodeError) -> io::Error {
    let kind = match error.kind() {
        DecodeErrorKind::Malformed => io::ErrorKind::InvalidData,
        DecodeErrorKind::Unsupported => io::ErrorKind::Unsupported,
        DecodeErrorKind::Io => std::error::Error::source(&error)
            .and_then(|cause| cause.downcast_ref::<io::Error>())
            .map_or(io::ErrorKind::Other, io::Error::kind),
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error)
}

/// Convert a panic in the linked tile decoder into its established `String`
/// error channel. Successful decode results pass through unchanged.
fn linked_decode<T>(
    operation: &'static str,
    f: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(format!("linked elivagar {operation} decoder panicked")),
    }
}

/// A failed read through [`Cursor`]: either protohoggr's own malformed-wire
/// kind, or a panic in the linked decoder contained at the seam. protohoggr's
/// `WireError` is a closed set of kinds, so the panic needs its own variant.
#[derive(Clone, Copy, Debug)]
pub enum WireError {
    Wire(protohoggr::WireError),
    Panicked(&'static str),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wire(error) => error.fmt(f),
            Self::Panicked(operation) => write!(f, "wire format error: {operation}"),
        }
    }
}

impl std::error::Error for WireError {}

pub type WireResult<T> = Result<T, WireError>;

fn linked_wire<T>(
    operation: &'static str,
    f: impl FnOnce() -> protohoggr::WireResult<T>,
) -> WireResult<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result.map_err(WireError::Wire),
        Err(_) => Err(WireError::Panicked(operation)),
    }
}

/// Panic-contained cursor surface used to read archive tile bytes.
#[derive(Clone, Debug)]
pub struct Cursor<'a>(protohoggr::Cursor<'a>);

impl<'a> Cursor<'a> {
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self(protohoggr::Cursor::new(data))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn read_varint(&mut self) -> WireResult<u64> {
        linked_wire("protobuf varint decoder panicked", || self.0.read_varint())
    }

    pub fn read_tag(&mut self) -> WireResult<Option<(u32, u32)>> {
        linked_wire("protobuf tag decoder panicked", || self.0.read_tag())
    }

    pub fn read_len_delimited(&mut self) -> WireResult<&'a [u8]> {
        linked_wire("protobuf field decoder panicked", || {
            self.0.read_len_delimited()
        })
    }
}

/// One directory run as brokkr's consumers walk it: `run_length` tiles from
/// `tile_id`, all addressing the payload at `offset`/`length` in the tile data
/// section. Built only from elivagar's validated `TileRun`s, so the run length
/// is never zero and the ids never pass the last z31 tile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawDirEntry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

impl RawDirEntry {
    #[must_use]
    pub const fn blob(&self) -> BlobRef {
        BlobRef {
            offset: self.offset,
            length: self.length,
        }
    }
}

/// The run of a sorted, non-overlapping run list that addresses `tile_id`.
#[must_use]
pub fn find_entry(runs: &[RawDirEntry], tile_id: u64) -> Option<RawDirEntry> {
    let index = runs.partition_point(|run| run.tile_id <= tile_id);
    let run = *runs.get(index.checked_sub(1)?)?;
    (tile_id - run.tile_id < u64::from(run.run_length)).then_some(run)
}

/// Panic-contained facade over elivagar's archive reader. It reads by
/// position into owned buffers, so a file truncated under it is an I/O error
/// rather than a SIGBUS.
pub struct ArchiveView(Archive);

impl ArchiveView {
    pub fn open(path: &Path) -> io::Result<Self> {
        linked_io("PMTiles archive reader", || {
            Archive::open(path).map_err(decode_io)
        })
        .map(Self)
    }

    pub fn header(&self) -> &Header {
        self.0.header()
    }

    pub fn min_zoom(&self) -> u8 {
        self.0.header().min_zoom
    }

    pub fn max_zoom(&self) -> u8 {
        self.0.header().max_zoom
    }

    pub fn num_addressed(&self) -> u64 {
        self.0.header().addressed_tiles
    }

    /// Why MVT-parsing workflows cannot read this archive's tiles, if they
    /// cannot: elivagar's one readability decision over the header.
    pub fn tile_format_refusal(&self) -> Option<String> {
        self.0.tile_decoder().err().map(|error| error.to_string())
    }

    /// Every tile run, sorted, from elivagar's validated directory tree: leaf
    /// pointers inside the leaf section, blobs inside tile data, runs inside
    /// their routing interval.
    pub fn read_all_runs(&self) -> io::Result<Vec<RawDirEntry>> {
        linked_io("PMTiles directory decoder", || {
            let runs = self.0.read_all_runs().map_err(decode_io)?;
            Ok(runs
                .as_slice()
                .iter()
                .map(|run| RawDirEntry {
                    tile_id: run.start().raw(),
                    offset: run.blob().offset,
                    length: run.blob().length,
                    run_length: run.run_length().get(),
                })
                .collect())
        })
    }

    /// The stored payload, still compressed.
    pub fn raw_blob(&self, blob: BlobRef) -> io::Result<Cow<'_, [u8]>> {
        linked_io("PMTiles blob reader", || {
            self.0.raw_blob(blob).map_err(decode_io)
        })
    }

    /// One payload, decompressed by the header's tile compression.
    pub fn read_tile(&self, blob: BlobRef) -> io::Result<Vec<u8>> {
        linked_io("PMTiles tile reader", || {
            self.0.read_tile(blob).map_err(decode_io)
        })
    }

    pub fn metadata(&self) -> io::Result<String> {
        linked_io("PMTiles metadata decoder", || {
            self.0.metadata().map_err(decode_io)
        })
    }
}

/// The tile id of a coordinate brokkr already bounded (enumerated from a zoom's
/// own matrix, or checked on entry). Panics on an out-of-range coordinate.
#[must_use]
pub fn xy_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
    TileId::at(z, x, y).raw()
}

/// The tile id of a coordinate from outside, refused when `z` passes z31 or
/// `x`/`y` leave the zoom's matrix.
pub fn checked_tile_id(z: u8, x: u32, y: u32) -> io::Result<u64> {
    TileId::from_coord(z, x, y).map(TileId::raw)
}

/// `(z, x, y)` of a tile id. An id past the last z31 tile is no address; it
/// reports zoom 32 with zero coordinates, which no archive names.
#[must_use]
pub fn tile_id_to_zxy(tile_id: u64) -> (u8, u32, u32) {
    TileId::new(tile_id).map_or((ARCHIVE_MAX_ZOOM + 1, 0, 0), TileId::coord)
}

/// The tile id that begins the zoom after the one holding `tile_id`, or
/// `u64::MAX` past the last z31 tile.
#[must_use]
pub fn next_zoom_boundary(tile_id: u64) -> u64 {
    TileId::new(tile_id).map_or(u64::MAX, |id| zoom_ids(id.zoom()).1)
}

/// Decode one MVT tile through the linked structural decoder. Every consumer
/// carries decode failures as text, so the typed error is formatted here.
pub fn decode_detail_tile(data: &[u8], strictness: Strictness) -> Result<DetailTile, String> {
    linked_decode("MVT tile", || {
        elivagar::tile_detail::decode_detail_tile(data, strictness).map_err(|e| e.to_string())
    })
}

/// Decode one MVT feature through the linked structural decoder.
pub fn decode_detail_feature(
    data: &[u8],
    keys: &[Arc<str>],
    values: &[DetailAttr],
    strictness: Strictness,
) -> Result<DetailFeature, String> {
    linked_decode("MVT feature", || {
        elivagar::tile_detail::decode_detail_feature(data, keys, values, strictness)
            .map_err(|e| e.to_string())
    })
}

/// Decode one MVT value through the linked structural decoder.
pub fn decode_detail_attr(data: &[u8], strictness: Strictness) -> Result<DetailAttr, String> {
    linked_decode("MVT value", || {
        elivagar::tile_detail::decode_detail_attr(data, strictness).map_err(|e| e.to_string())
    })
}

/// Read one protobuf varint while preserving the caller's byte cursor. The
/// mutation tool uses this for field surgery, where unknown fields must retain
/// their original payload bytes and cannot be normalized through tile decode.
pub(crate) fn read_varint(input: &[u8], at: &mut usize) -> io::Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *input.get(*at).ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "truncated protobuf varint")
        })?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "protobuf varint overflows u64",
    ))
}
