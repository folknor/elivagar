//! `brokkr regress` - the two-archive semantic diff (tier-3 attribution).
//!
//! Native brokkr code over the linked elivagar crate since the corpus redesign;
//! it used to shell out to `elivagar regress`, which no longer exists. brokkr
//! owns the adjudication, elivagar owns the pipeline and the decode surface.
//!
//! What it computes: a semantic diff of two PMTiles archives (MVT + gzip only).
//! Every tile is reduced to a canonical form - layers sorted, features sorted,
//! merged-feature components sorted - which erases intra-layer feature order and
//! nothing else. That one dimension is the only thing the pipeline deliberately
//! leaves unconstrained across archives: within a run, record order is total, so
//! two builds of the same commit are byte-identical; two builds of *different*
//! commits can legitimately reorder features (a paint-rank table change, say)
//! with no semantic difference, and that is what the canonicalization absorbs.
//! Everything surviving it is classified - tiles/layers added or removed, extent
//! mismatches, missing/added features, bit-exact attribute changes, and
//! matched-feature geometry moves split into tolerance vs structural.
//!
//! There is deliberately **no comparability gate** and no baseline registry.
//! regress reads no provenance contract by design - it is the attribution
//! instrument, and its legitimate uses include cross-contract comparisons
//! (adjudicating artifact-active vs computed output, pricing an intended config
//! change). A hard refusal here would block those and push people back to a raw
//! spelling that no longer exists. Comparability is the caller's
//! responsibility; `brokkr pmtiles-inspect` reads the provenance blocks, and a
//! cross-variant comparison reports six-figure diffs on two correct builds.
//!
//! The standing output gate is `brokkr pmtiles-corpus check`, not this.

use std::path::Path;

use crate::error::DevError;
use crate::output;

pub(crate) mod compare;
pub(crate) mod engine;
pub(crate) mod geometry;
pub(crate) mod overlay;
pub(crate) mod pairing;
pub(crate) mod prepared;
pub(crate) mod report;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

pub use report::RegressConfig;

/// A regress run could not be completed - an unreadable archive, a tile that
/// will not decode, an overlay that will not write.
///
/// This is exit **3**, never 1. Exit 1 is the diff *verdict*, and a caller that
/// cannot separate "content differs" from "regress never ran" reads a truncated
/// archive as a regression, with only stderr to say otherwise. 2 stays reserved
/// for clap's usage errors, so the three outcomes never collide. The same code
/// covers failures before the diff starts - lock, bootstrap, archive
/// resolution - via `cmd::operational` at the dispatch site.
pub(crate) const INCOMPLETE_EXIT: i32 = 3;

fn failed(message: &str) -> DevError {
    output::error(message);
    DevError::ExitCode(INCOMPLETE_EXIT)
}

/// Run the diff and print the report; `Err(ExitCode(1))` on a failing verdict,
/// `Err(ExitCode(3))` if the run could not be completed at all.
///
/// Both archives come from elivagar, so the decoder runs **strict**: a wire
/// field the MVT schema does not name is an error rather than something skipped
/// silently. The tolerant policy is `compare-tiles`', for foreign producers.
#[allow(clippy::too_many_arguments)]
pub fn run(
    current: &Path,
    comparand: &Path,
    tol: i32,
    max_moved: u64,
    max_examples: usize,
    overlay_dir: Option<&Path>,
    overlay_max: Option<usize>,
    json: bool,
) -> Result<(), DevError> {
    let cfg = RegressConfig {
        tol,
        max_moved,
        max_examples,
    };
    output::run_msg(&format!(
        "regress {} against {}",
        current.display(),
        comparand.display()
    ));

    let report = engine::regress(current, comparand, &cfg, super::eliv::Strictness::Strict)
        .map_err(|e| failed(&format!("regress failed: {e}")))?;
    let passed = report.passed(&cfg);

    if json {
        let json = serde_json::to_string(&report.to_json(passed))
            .map_err(|e| failed(&format!("serialize regress report: {e}")))?;
        output::plain(&json);
    } else {
        report.print_text();
    }

    if let Some(dir) = overlay_dir {
        let written = overlay::dump_overlays(
            current,
            comparand,
            &report,
            &cfg,
            &overlay::OverlayOptions {
                dir,
                max: overlay_max.unwrap_or(super::constants::DEFAULT_OVERLAY_MAX),
                background: super::constants::REGRESS_OVERLAY_BACKGROUND,
                strictness: super::eliv::Strictness::Strict,
            },
        )
        .map_err(|e| failed(&format!("write overlays: {e}")))?;
        output::run_msg(&format!(
            "overlay: {written} tile(s) -> {}",
            output::path(dir)
        ));
    }

    if passed {
        Ok(())
    } else {
        // Exit 1 is the verdict, not an error: the report already went to
        // stdout and is the message.
        Err(DevError::ExitCode(1))
    }
}
