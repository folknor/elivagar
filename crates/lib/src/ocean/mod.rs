// Ocean shapefile processing.
//
// Reads a water-polygons-split-3857 shapefile and emits tiles through the
// shared int_ocean pyramid engine: quantize-early to max-zoom pixel space,
// then per-tile integer boolean clipping. No scanline fill, no
// point-in-polygon, no Sutherland-Hodgman, no LandMask.
//
// One job per module:
// - `policy`: the artifact policy version and the pyramid parameters ocean
//   selects.
// - `key`: artifact identity - the digest type, shapefile pairs, the source
//   and artifact keys and their wire shapes.
// - `shapefile`: the ESRI Polygon reader, free of sort and pyramid deps.
// - `emit`: one pass over one shapefile into sort chunks, and its counters.
// - `plan`: the pass structure; `band`: band ownership between an extract
//   and the world artifact; `artifact`: artifact admission and directory
//   access; `retention`: keeping the outgoing artifact one generation deep.

pub(crate) mod artifact;
pub(crate) mod band;
mod emit;
mod key;
pub(crate) mod plan;
mod policy;
pub(crate) mod retention;
mod shapefile;

pub(crate) use artifact::{OceanArtifact, OceanTiles};
pub(crate) use band::OceanBand;
pub(crate) use emit::{
    emit_ocean_counters, process_ocean_shapefile_checked, process_ocean_shapefile_with_hashes,
    reset_ocean_counters,
};
pub(crate) use key::OceanShapefileHashes;
pub use key::{OceanArtifactKey, OceanSourceKey, ShapefilePair, Xxh128};
pub(crate) use plan::{OceanPass, OceanPlan};
#[cfg(test)]
pub use policy::OCEAN_POLICY_VERSION;
pub(crate) use shapefile::ShapefileError;
