//! The relation prepass: a relations-only read run on its own thread at
//! phase start, producing the membership plan and boundary metadata the way
//! phase consumes.
//!
//! It lives with phase12 rather than with the relation tail
//! (`pipeline::relations`) because its consumer is the way phase: the tail
//! reads the finished way index, the prepass feeds the plans that build it.
//! It runs even when membership is injected, because boundary metadata comes
//! only from here.

use rustc_hash::FxHashSet;

use crate::shortbread::{self, Tags};
use pbfhogg::{BlobFilter, Element, ElementReader, MemberId};

use super::super::PipelineError;
use super::super::boundary_prescan::{BoundaryWayMeta, fold_relation};

pub(super) struct RelationPlan {
    pub(super) needed_ways: FxHashSet<i64>,
    pub(super) boundary_way_meta: BoundaryWayMeta,
    /// Size of the union of `needed_ways` with the member ways of
    /// mp/boundary relations that FAIL the shortbread match - i.e. members
    /// of ALL type=multipolygon/boundary relations. This is exactly the set
    /// an enrichment-time producer with no shortbread knowledge would mark
    /// (superset semantics), so `superset_ways_count / needed_ways.len()`
    /// is the superset inflation factor. Count only; the set is transient.
    pub(super) superset_ways_count: usize,
}

type PrepassHandle = std::thread::JoinHandle<Result<RelationPlan, PipelineError>>;

/// The one-shot prepass thread. Joined at the first way block, or at the end
/// of the read when the input has no way blocks; dropping it unjoined (an
/// error return) still waits for the thread.
pub(super) struct Prepass {
    handle: Option<PrepassHandle>,
}

impl Prepass {
    pub(super) fn spawn(pbf_path: &std::path::Path, decode_threads: usize) -> Self {
        let path = pbf_path.to_path_buf();
        Self {
            handle: Some(std::thread::spawn(move || {
                prepass_relation_plan(&path, decode_threads)
            })),
        }
    }

    /// Wait for the plan. Panics if already joined.
    pub(super) fn join(&mut self) -> Result<RelationPlan, PipelineError> {
        join_handle(self.handle.take().expect("relation prepass already joined"))
    }

    /// Join the thread if no way block ever did, propagating its error.
    pub(super) fn finish(mut self) -> Result<(), PipelineError> {
        match self.handle.take() {
            Some(handle) => join_handle(handle).map(drop),
            None => Ok(()),
        }
    }
}

fn join_handle(handle: PrepassHandle) -> Result<RelationPlan, PipelineError> {
    handle
        .join()
        .map_err(|_| PipelineError::internal("relation prepass thread panicked"))?
}

impl Drop for Prepass {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

#[hotpath::measure]
fn prepass_relation_plan(
    pbf_path: &std::path::Path,
    decode_threads: usize,
) -> Result<RelationPlan, PipelineError> {
    let start = std::time::Instant::now();
    let reader = ElementReader::from_path(pbf_path)
        .map_err(|e| PipelineError::failed("relation prepass: failed to open PBF").with_cause(e))?
        .with_blob_filter(BlobFilter::only_relations())
        .decode_threads(decode_threads);

    let mut needed_ways: FxHashSet<i64> = FxHashSet::default();
    let mut boundary_way_meta: BoundaryWayMeta = BoundaryWayMeta::default();
    // Members of ALL mp/boundary relations, shortbread match or not - what
    // an enrichment-time superset bitmap would contain. Only the count is
    // kept; the set is dropped before the plan is returned.
    let mut superset_ways: FxHashSet<i64> = FxHashSet::default();
    let mut matched_relations: u64 = 0;
    let blocks = reader.into_blocks_pipelined().map_err(|e| {
        PipelineError::failed("relation prepass: PBF pipeline start failed").with_cause(e)
    })?;
    for block_result in blocks {
        let block = block_result.map_err(|e| {
            PipelineError::failed("relation prepass: PBF read failed").with_cause(e)
        })?;
        block.for_each_element(|element| {
            let Element::Relation(rel) = element else {
                return;
            };
            let tags: smallvec::SmallVec<[(&str, &str); 16]> = rel.tags().collect();
            let tag_helper = Tags(&tags);
            // Boundary metadata deliberately has no type gate: canonical
            // Shortbread resolves boundary=administrative/disputed membership
            // regardless of the relation's type tag.
            fold_relation(
                &tag_helper,
                rel.members().filter_map(|member| match member.id {
                    MemberId::Way(way_id) => Some(way_id),
                    _ => None,
                }),
                &mut boundary_way_meta,
            );
            if !shortbread::is_area_relation(&tag_helper) {
                return;
            }
            let matched = shortbread::area_relation_matches(&tag_helper).is_some();
            if matched {
                matched_relations += 1;
            }
            for member in rel.members() {
                if let MemberId::Way(way_id) = member.id {
                    superset_ways.insert(way_id);
                    if matched {
                        needed_ways.insert(way_id);
                    }
                }
            }
        });
    }
    needed_ways.shrink_to_fit();
    let superset_ways_count = superset_ways.len();
    drop(superset_ways);

    eprintln!(
        "  Relation prepass: {:.1}s ({} matching relations, {} member ways, {} superset ways)",
        start.elapsed().as_secs_f64(),
        matched_relations,
        needed_ways.len(),
        superset_ways_count,
    );
    Ok(RelationPlan {
        needed_ways,
        boundary_way_meta,
        superset_ways_count,
    })
}
