//! elivagar's development kit: the one owner of brokkr.toml host
//! configuration, the git questions archive naming asks, and the durable
//! tilegen archive store, plus the tilegen block and its elivagar argv.
//!
//! `elivagar-corpus` reads archives through [`archive_store`]; the
//! `elivagar-devkit` binary writes them (`tilegen`) and builds the world
//! ocean artifact (`ocean-build`), resolves a stored archive's path
//! (`archive`) and removes the elivagar scratch brokkr's `clean` used to
//! (`clean`). Ported from brokkr's `src/elivagar/` and `src/clean.rs`, where
//! these ran as `brokkr tilegen`, `brokkr ocean-build`, the archive resolver
//! of the pmtiles-inspect/diag/svg/verify wrappers, and `brokkr clean`.

pub mod archive_store;
pub mod clean;
pub mod config;
mod error;
pub mod git;
pub mod tilegen;

pub use error::DevError;
