//! What a `--skip-to` resume must match, and where it is recorded.
//!
//! The chunk directory is built by two producers in sequence, and each one
//! records its own identity once its chunks are on disk:
//!
//! - [`PbfCheckpoint`] (`checkpoint.txt`) is written after phase12 flushes.
//!   It records what produced the PBF chunks - the input hash, the producer
//!   config and the chunk codec - plus what phase12 derived: the data
//!   extent, the PBF chunk count and the effective paths. Phase12 reads no
//!   ocean input, so no ocean identity belongs here.
//! - [`ChunkCompletion`] (`chunks_complete.json`) is written after the ocean
//!   phase and the final flush. It proves the chunk set is complete and
//!   records what produced the ocean chunks.
//!
//! Every resume is admitted against the PBF record. `--skip-to ocean`
//! re-runs the ocean, so it may change the ocean freely: it clears the
//! completion record before touching the chunk set and the run writes a new
//! one when its ocean pass completes. `--skip-to sort` and `assemble` reuse
//! the ocean chunks too, so they also require the completion record and its
//! ocean identity to match this run.
//!
//! The bounds, chunk counts and effective paths are inherited, never
//! compared: they are outputs of the producer, not settings of the run.

use serde::{Deserialize, Serialize};

use super::{PipelineError, SkipTo};
use crate::debug::emit_counter;
use crate::geometry::projection::MercBbox;
use crate::ocean::{OceanSourceKey, Xxh128};
use crate::sort::ChunkCompression;

const CHECKPOINT_FILE: &str = "checkpoint.txt";
const COMPLETION_FILE: &str = "chunks_complete.json";

/// v11 splits the record by producer: the PBF checkpoint carries no ocean
/// identity, and the ocean chunks' identity moved to the completion record
/// written after the ocean phase, which replaced the bare `sort_chunks.count`.
/// That identity is the ocean source key plus whether the run emitted only
/// the artifact's boundary band; the artifact's compression level and the
/// low-zoom selection are no longer recorded, because the chunks hold
/// geometry that neither changes (the selection is a function of the source
/// key and the zoom window).
///
/// v10 changes no field, only what the recorded identity means: computed
/// ocean constructs each pass on its canonical grid (z7 and z14, roots at
/// the pass's first zoom) and the window only truncates emission, and an
/// artifact-served run judges band ownership on both pass grids whatever the
/// spelling. A v9 checkpoint from a narrowed window, or from a single
/// `z0-z14` shapefile against an artifact, carries the same keys and producer
/// config over chunks built the old way, so it is refused rather than reused.
///
/// v9 recorded ocean mode, low-zoom source, and source identity through the
/// serde-tagged contract shared with provenance. v8 includes inline way
/// coordinates in the checkpoint extent for locations-on-ways inputs, whose
/// producer may omit untagged node elements. v7 is the first JSON encoding,
/// recording the input hash, the producer config, effective paths, and the
/// sort chunk storage codec. v6 added computed-ocean source identity; v5
/// recorded computed ocean as the bare string "computed", so chunks built
/// from one shapefile could be reused by a resume naming another. v4 and
/// earlier were whitespace-positional, where a miscount is a silent
/// misparse. None of them is accepted; re-run a full tilegen.
///
/// The version moves whenever chunks built under unchanged recorded identity
/// would differ. It is shared by both records.
pub(super) const CHECKPOINT_VERSION: u32 = 11;

/// What produced the PBF chunks, and what phase12 derived from the input.
///
/// Chunks on disk are the product of one PBF and one producer config, and
/// nothing in a later invocation reveals either. Resuming against a different
/// input, or a different zoom range or fanout cap, silently mixes two
/// contracts into one archive and then stamps it with provenance describing
/// only the second - the metadata would be confidently wrong, which is worse
/// than absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PbfCheckpoint {
    version: u32,
    /// Data extent in Mercator, `[min_x, min_y, max_x, max_y]`, with the
    /// ocean overlap buffer. Feeds ocean selection and the header bounds.
    bounds: [f64; 4],
    /// Chunks phase12 wrote; `--skip-to ocean` keeps exactly these.
    pub(crate) chunks: usize,
    pub(crate) input_xxh3_128: Xxh128,
    /// `provenance::producer_config` of the run that wrote the chunks.
    /// Compared as JSON so the float factor needs no float-equality dance
    /// and a mismatch names the field.
    pub(crate) producer_config: serde_json::Value,
    /// Storage format, not producer provenance, but readers must match it to
    /// decode the files: single-partition chunk files carry no codec magic.
    pub(crate) sort_chunk_compression: ChunkCompression,
    pub(crate) effective: crate::provenance::Effective,
}

impl PbfCheckpoint {
    pub(crate) fn new(
        bounds: &MercBbox,
        chunks: usize,
        input_xxh3_128: Xxh128,
        producer_config: serde_json::Value,
        sort_chunk_compression: ChunkCompression,
        effective: crate::provenance::Effective,
    ) -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            bounds: [bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y],
            chunks,
            input_xxh3_128,
            producer_config,
            sort_chunk_compression,
            effective,
        }
    }

    pub(crate) fn bounds(&self) -> MercBbox {
        let [min_x, min_y, max_x, max_y] = self.bounds;
        MercBbox {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    pub(crate) fn save(&self, tmp_dir: &std::path::Path) -> Result<(), PipelineError> {
        write_json(&tmp_dir.join(CHECKPOINT_FILE), self)
    }

    pub(crate) fn load(tmp_dir: &std::path::Path) -> Result<Self, PipelineError> {
        let path = tmp_dir.join(CHECKPOINT_FILE);
        let content = std::fs::read_to_string(&path).map_err(|e| {
            emit_counter("resume_refusal_missing_checkpoint", 1);
            PipelineError(format!(
                "no checkpoint in {}: {e} (run a full tilegen first)",
                tmp_dir.display()
            ))
        })?;
        let stale = || {
            emit_counter("resume_refusal_stale_checkpoint", 1);
            PipelineError(format!(
                "checkpoint in {} predates v{CHECKPOINT_VERSION} or is malformed; \
                 re-run a full tilegen to regenerate it",
                tmp_dir.display()
            ))
        };
        parse_versioned(&content).ok_or_else(stale)
    }
}

/// What produced the ocean chunks.
///
/// A computed run emits every ocean tile; an artifact-served run emits only
/// the boundary band and leaves the interior to the artifact, so the two
/// chunk sets differ under the same source key. The artifact's compression
/// level is deliberately absent: chunks hold geometry, not encoded tiles,
/// and the artifact itself is admitted against this run's level at entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum OceanChunks {
    None,
    Computed { source_key: OceanSourceKey },
    Band { source_key: OceanSourceKey },
}

/// Proof that the ocean phase and the final flush completed, and over what.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChunkCompletion {
    version: u32,
    /// Total chunks, PBF and ocean, checked against the directory scan.
    pub(crate) chunks: usize,
    pub(crate) ocean: OceanChunks,
}

impl ChunkCompletion {
    pub(crate) fn new(chunks: usize, ocean: OceanChunks) -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            chunks,
            ocean,
        }
    }

    pub(crate) fn save(&self, tmp_dir: &std::path::Path) -> Result<(), PipelineError> {
        write_json(&tmp_dir.join(COMPLETION_FILE), self)
    }

    /// A skip-to-sort/assemble resume requires proof that the ocean pass
    /// finished. The record is written only after that pass; absence or
    /// corruption means the chunk set may be partial and must not be
    /// assembled.
    pub(crate) fn load(tmp_dir: &std::path::Path) -> Result<Self, PipelineError> {
        let path = tmp_dir.join(COMPLETION_FILE);
        let content = std::fs::read_to_string(&path).map_err(|error| {
            emit_counter("resume_refusal_missing_sort_chunk_count", 1);
            PipelineError(format!(
                "chunk completion record {} is missing or unreadable ({error}); the ocean phase may not have completed, so \
                 --skip-to sort or assemble is unsafe; rerun from --skip-to ocean or do a full tilegen",
                path.display()
            ))
        })?;
        parse_versioned(&content).ok_or_else(|| {
            emit_counter("resume_refusal_invalid_sort_chunk_count", 1);
            PipelineError(format!(
                "chunk completion record {} is invalid or predates v{CHECKPOINT_VERSION}; \
                 --skip-to sort or assemble is unsafe; rerun from --skip-to ocean or do a full tilegen",
                path.display()
            ))
        })
    }

    /// Invalidate a previous completion record before the chunk set changes.
    /// A failed ocean pass must leave no record that makes its partial
    /// output appear complete.
    pub(crate) fn clear(tmp_dir: &std::path::Path) -> Result<(), PipelineError> {
        let path = tmp_dir.join(COMPLETION_FILE);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                emit_counter("resume_refusal_sort_chunk_count_clear", 1);
                Err(PipelineError(format!(
                    "could not clear prior chunk completion record {} before changing the chunk set: {error}",
                    path.display()
                )))
            }
        }
    }
}

/// This run's side of every resume comparison.
pub(crate) struct RunIdentity {
    pub(crate) input_xxh3_128: Xxh128,
    pub(crate) producer_config: serde_json::Value,
    pub(crate) sort_chunk_compression: ChunkCompression,
    pub(crate) ocean: OceanChunks,
}

/// The records an admitted resume inherits.
pub(crate) struct Resume {
    pub(crate) pbf: PbfCheckpoint,
    /// The completion record, present exactly for `sort` and `assemble`.
    pub(crate) completion: Option<ChunkCompletion>,
}

/// Admit a resume at `skip`, or refuse it with the reason. The one place a
/// resume is compared against the run; each record is read once.
pub(crate) fn admit(
    tmp_dir: &std::path::Path,
    skip: SkipTo,
    run: &RunIdentity,
) -> Result<Resume, PipelineError> {
    let pbf = PbfCheckpoint::load(tmp_dir)?;
    if pbf.sort_chunk_compression != run.sort_chunk_compression {
        emit_counter("resume_refusal_sort_compression", 1);
        return Err(PipelineError(format!(
            "checkpoint sort chunks use {:?} compression but this run requests {:?}; \
             --compress-sort-chunks must match the setting that wrote the chunks, so \
             --skip-to cannot resume; re-run a full tilegen",
            pbf.sort_chunk_compression, run.sort_chunk_compression
        )));
    }
    if pbf.input_xxh3_128 != run.input_xxh3_128 {
        emit_counter("resume_refusal_input", 1);
        return Err(PipelineError(format!(
            "checkpoint was built from a different PBF (chunks: {}, this run: {}); \
             --skip-to would mix two inputs into one archive - re-run a full tilegen",
            pbf.input_xxh3_128, run.input_xxh3_128,
        )));
    }
    // The chunks encode the producer config: zoom range, fanout caps,
    // simplification. They cannot be reinterpreted under different settings,
    // and reusing them would record settings the tiles were never built under.
    if pbf.producer_config != run.producer_config {
        emit_counter("resume_refusal_producer_config", 1);
        let diffs =
            crate::provenance::producer_config_diff(&pbf.producer_config, &run.producer_config);
        return Err(PipelineError(format!(
            "checkpoint chunks were produced under a different config: {}; \
             --skip-to cannot reinterpret them - re-run a full tilegen",
            diffs.join("; "),
        )));
    }
    let completion = match skip {
        SkipTo::Ocean => None,
        SkipTo::Sort | SkipTo::Assemble => {
            let completion = ChunkCompletion::load(tmp_dir)?;
            if completion.ocean != run.ocean {
                emit_counter("resume_refusal_ocean", 1);
                return Err(PipelineError(
                    "the ocean chunks on disk were produced from a different ocean input than \
                     this run names; --skip-to sort and assemble reuse them, so resume from \
                     --skip-to ocean to rebuild the ocean instead"
                        .to_string(),
                ));
            }
            Some(completion)
        }
    };
    Ok(Resume { pbf, completion })
}

fn write_json<T: Serialize>(path: &std::path::Path, record: &T) -> Result<(), PipelineError> {
    let content = serde_json::to_string(record).map_err(|error| {
        PipelineError(format!("could not serialize {}: {error}", path.display()))
    })?;
    std::fs::write(path, content)?; // io::Error message is sufficient context.
    Ok(())
}

/// Parse a record, refusing any other version before reading its fields:
/// an older layout would otherwise surface as a confusing field error.
fn parse_versioned<T: serde::de::DeserializeOwned>(content: &str) -> Option<T> {
    let doc: serde_json::Value = serde_json::from_str(content).ok()?;
    if doc.get("version").and_then(serde_json::Value::as_u64) != Some(u64::from(CHECKPOINT_VERSION))
    {
        return None;
    }
    serde_json::from_value(doc).ok()
}
