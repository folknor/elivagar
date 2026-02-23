// Elivagar — Shortbread vector tile generator.
//
// Named after the rivers of Niflheim. Reads OSM PBF files and produces
// PMTiles v3 archives with the Shortbread schema (26 layers).

pub mod geometry;
pub mod multipolygon;
pub mod mvt;
pub mod node_index;
pub mod ocean;
mod pipeline;
pub mod pmtiles_writer;
pub mod pois;
pub mod shortbread;
pub mod sort;
pub mod way_index;
pub mod wire_format;

pub use pipeline::{run, TilegenConfig};
