// Elivagar — Shortbread vector tile generator.
//
// Named after the rivers of Niflheim. Reads OSM PBF files and produces
// PMTiles v3 archives with the Shortbread schema (26 layers).

pub(crate) mod geometry;
pub(crate) mod multipolygon;
pub(crate) mod mvt;
pub(crate) mod node_index;
pub(crate) mod ocean;
mod pipeline;
pub mod pmtiles_writer;
pub(crate) mod pois;
pub(crate) mod shortbread;
pub(crate) mod sort;
pub(crate) mod way_index;
pub(crate) mod wire_format;

pub use pipeline::{run, PipelineError, SkipTo, TilegenConfig};
