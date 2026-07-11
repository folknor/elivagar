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
//!     max_zoom: 14,
//!     ocean_shapefile: None,
//!     ocean_simplified_shapefile: None,
//!     skip_to: None,
//!     in_memory: false,
//!     compression_level: 6,
//!     force_sorted: false,
//!     allow_unsafe_flat_index: false,
//!     threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
//!     way_inflight_budget: 0,
//!     assemble_batch_budget: 0,
//!     sort_chunk_size: 0,
//!     locations_on_ways: false,
//!     tile_format: elivagar::TilePayloadFormat::Mvt,
//!     tile_compression: elivagar::TileCompression::Gzip,
//!     compress_sort_chunks: elivagar::sort::ChunkCompression::None,
//!     seam_reconcile_layers: {
//!         let mut m = [0u8; elivagar::shortbread::Layer::count()];
//!         m[elivagar::shortbread::Layer::Boundaries as usize] = 8;
//!         m
//!     },
//!     fanout_caps: [0; elivagar::shortbread::Layer::count()],
//!     polygon_simplify_factor: 1.0,
//! };
//! elivagar::run(&config).expect("pipeline failed");
//! ```
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
//! # PMTiles writer
//!
//! The [`pmtiles_writer`] module is also public for standalone use. It writes
//! clustered PMTiles v3 archives with Hilbert-ordered tile IDs and content
//! deduplication.

extern crate alloc;

pub(crate) mod debug;
pub(crate) mod geometry;
pub mod inspect;
pub(crate) mod mlt;
pub(crate) mod multipolygon;
pub(crate) mod mvt;
pub mod node_index;
pub(crate) mod ocean;
mod pipeline;
pub mod pmtiles_reader;
pub mod pmtiles_writer;
pub(crate) mod pois;
pub mod regress;
pub mod shortbread;
pub mod sort;
pub mod svg;
pub mod verify;
pub(crate) mod way_index;
pub(crate) mod wire_format;

pub use pipeline::TileCompression;
pub use pipeline::TilePayloadFormat;
pub use pipeline::{PipelineError, SkipTo, TilegenConfig, run};
