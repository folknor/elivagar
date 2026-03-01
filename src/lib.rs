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
//!     threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
//!     way_inflight_budget: 0,
//!     rel_batch_budget: 0,
//!     assemble_batch_budget: 0,
//!     sort_chunk_size: 0,
//! };
//! elivagar::run(&config).expect("pipeline failed");
//! ```
//!
//! # Pipeline phases
//!
//! 1. **PBF read** — single-pass read building node/way indices and emitting
//!    sort records for matched features.
//! 2. **Ocean** — ocean shapefile processing (optional). Generates water polygon
//!    tiles from an ESRI shapefile.
//! 3. **Sort** — external merge sort of all records by Hilbert tile ID.
//! 4. **Assembly** — MVT protobuf encode, gzip compress, and write PMTiles archive.
//!
//! The [`SkipTo`] enum allows resuming from a checkpoint, reusing sort chunks
//! from a previous run.
//!
//! # PMTiles writer
//!
//! The [`pmtiles_writer`] module is also public for standalone use. It writes
//! clustered PMTiles v3 archives with Hilbert-ordered tile IDs and content
//! deduplication.

pub(crate) mod geometry;
pub(crate) mod multipolygon;
pub(crate) mod mvt;
pub mod node_index;
pub(crate) mod ocean;
mod pipeline;
pub mod pmtiles_writer;
pub(crate) mod pois;
pub(crate) mod shortbread;
pub(crate) mod sort;
pub(crate) mod way_index;
pub(crate) mod wire_format;

pub use pipeline::{run, PipelineError, SkipTo, TilegenConfig};
