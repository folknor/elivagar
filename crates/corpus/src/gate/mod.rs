//! The gate code, ported from brokkr's `src/elivagar/` with its module tree
//! kept intact (`corpus/`, `regress/`, `compare_tiles` and the `eliv` seam),
//! so the sources stay diffable against brokkr's. The archive resolver lives
//! in `elivagar_devkit::archive_store`, beside the writer side tilegen uses.

pub(crate) mod compare_tiles;
pub(crate) mod constants;
pub(crate) mod corpus;
pub(crate) mod eliv;
pub(crate) mod regress;
pub(crate) mod stats;
