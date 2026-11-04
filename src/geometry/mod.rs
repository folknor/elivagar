// Geometry engine: projection, simplification, clipping for vector tile generation.
//
// All operations work in Mercator [0,1] coordinate space unless stated otherwise.
// Pure Rust aside from smallvec for inline small-vec returns.

pub mod clip;
pub(crate) mod int_ocean;
pub(crate) mod line_merge;
pub(crate) mod overlay;
pub mod polygon;
pub mod projection;
pub(crate) mod pyramid;
pub mod simplify;
pub(crate) mod surface;
pub mod tiles;

// ---------------------------------------------------------------------------
// Tests (see geometry_tests.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../geometry_tests.rs"]
mod tests;
