//! elivagar's development kit: the one owner of brokkr.toml host
//! configuration, the git questions archive naming asks, and the durable
//! tilegen archive store, plus the tilegen block and its elivagar argv.
//!
//! `elivagar-corpus` reads archives through [`archive_store`]; the
//! `elivagar-devkit` binary writes them (`tilegen`) and builds the world
//! ocean artifact (`ocean-build`). Ported from brokkr's `src/elivagar/`,
//! where these ran as `brokkr tilegen` and `brokkr ocean-build`.

pub mod archive_store;
pub mod config;
mod error;
pub mod git;
pub mod tilegen;

pub use error::DevError;
