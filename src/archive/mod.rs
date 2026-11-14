//! PMTiles v3 archives: the header codec, directories, the one reader and
//! the writer.
//!
//! - [`header`] - the 127-byte header, decoded losslessly, plus the
//!   readability decision ([`TileDecoder`]) and the writer's degree types.
//! - [`directory`] - the directory codec and the run types the reader hands
//!   out ([`TileRun`], [`TileRuns`], [`BlobRef`]).
//! - [`reader`] - [`Archive`], the only reader.
//! - [`writer`] - [`PmtilesWriter`].

pub mod directory;
pub mod header;
pub mod reader;
pub mod writer;

pub use directory::{BlobRef, TileRun, TileRuns};
pub use header::{
    Clustering, Compression, E7Bbox, E7Center, HEADER_LEN, Header, LonLatBbox, MapCenter, Section,
    TileDecoder, TileType,
};
pub use reader::{Archive, FileSource, MapSource, Source};
pub use writer::{
    DEFAULT_DEDUP_CAP, DedupStats, PmtilesConfig, PmtilesWriter, TileDataCompression,
    TileDataFormat, Tileset, VectorLayer,
};

pub(crate) use reader::gzip_decompress;
