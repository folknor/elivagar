//! Shortbread vector tile generator.
//!
//! Elivagar reads OpenStreetMap PBF files and produces
//! [PMTiles v3](https://github.com/protomaps/PMTiles) archives with the
//! [Shortbread](https://shortbread-tiles.org/) schema (26 layers).
//!
//! # Usage
//!
//! The primary entry point is [`run()`], which takes a [`TilegenConfig`] and
//! drives the full pipeline:
//!
//! ```no_run
//! let config = elivagar::TilegenConfig {
//!     pbf_path: "input.osm.pbf".into(),
//!     output_path: "output.pmtiles".into(),
//!     tmp_dir: "data/tilegen_tmp".into(),
//!     min_zoom: 0,
//!     max_zoom: elivagar::tiling::MAX_ZOOM,
//!     ocean: None,
//!     skip_to: None,
//!     in_memory: false,
//!     compression_level: 6,
//!     force_sorted: false,
//!     allow_unsafe_flat_index: false,
//!     threads: elivagar::resources::default_threads(),
//!     way_inflight_budget: None,
//!     assemble_batch_budget: None,
//!     dedup_cap: None,
//!     sort_chunk_size: None,
//!     locations_on_ways: false,
//!     tile_format: elivagar::TilePayloadFormat::Mvt,
//!     tile_compression: elivagar::TileCompression::Gzip,
//!     compress_sort_chunks: elivagar::sort::ChunkCompression::None,
//!     fanout_caps: elivagar::FanoutCaps::default(),
//!     polygon_simplify_factor: elivagar::SimplifyFactor::default(),
//! };
//! elivagar::run(&config).expect("pipeline failed");
//! ```
//!
//! For CLI runs, `TilegenConfig::threads` also sets the Rayon pool size. Phase12
//! derives the pbfhogg decode worker count from it, along with other worker
//! counts and in-flight ceilings, so it is not a process-wide thread cap.
//!
//! # Pipeline phases
//!
//! 1. **PBF read** - single-pass read building node/way indices and emitting
//!    sort records for matched features.
//! 2. **Ocean** - ocean shapefile processing (optional). Generates water polygon
//!    tiles from an ESRI shapefile.
//! 3. **Sort** - external merge sort of all records by Hilbert tile ID.
//! 4. **Assembly** - MVT protobuf encode, gzip compress, and write PMTiles archive.
//!
//! The [`SkipTo`] enum allows resuming from a checkpoint, reusing sort chunks
//! from a previous run.
//!
//! # PMTiles archives
//!
//! The [`archive`] module is also public for standalone use: the one reader
//! ([`archive::Archive`]), the header codec, and the writer
//! ([`archive::PmtilesWriter`]), which writes clustered PMTiles v3 archives
//! with Hilbert-ordered tile IDs and content deduplication.

extern crate alloc;

mod config;
pub use config::{FanoutCaps, OceanInputs, SimplifyFactor};
pub mod archive;
pub(crate) mod debug;
pub mod diag;
pub(crate) mod geometry;
pub mod inspect;
#[cfg(feature = "mlt")]
pub(crate) mod mlt;
pub(crate) mod multipolygon;
pub(crate) mod mvt;
pub mod node_index;
pub(crate) mod ocean;
mod pipeline;
pub(crate) use shortbread::pois;
pub mod provenance;
pub mod resources;
pub mod shortbread;
pub mod sort;
pub mod svg;
#[cfg(test)]
pub(crate) mod test_support;
pub mod tile_detail;
pub mod tiling;
pub mod verify;
pub(crate) mod way_index;
pub(crate) mod wire_format;

pub use pipeline::TileCompression;
pub use pipeline::TilePayloadFormat;
pub use pipeline::{
    ErrorKind, OceanMode, PipelineError, ResumeRefusal, SkipTo, TilegenConfig, ocean_build, run,
};
