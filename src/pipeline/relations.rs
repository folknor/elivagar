use smallvec::SmallVec;

use crate::geometry::pyramid::RingCapError;
use crate::geometry::{self, projection::Point};
use crate::multipolygon::{self, MemberWay, WayRole};
use crate::shortbread::LayerMatch;
use crate::shortbread::{self, Tags};
use crate::sort::{self, SortWriter};
use crate::tiling::ZoomRange;
use crate::way_index::{WayId, WayIndexReader};
use pbfhogg::MemberId;

use super::PipelineError;
use super::emit::{
    EmitCtx, FeatureGeometry, MultipolygonEmitScratch, PointEmitScratch, RecordSink, emit_match,
    enrich_polygon_matches, relation_shared_vertex_keys, unwrap_antimeridian_path,
};
use super::stats::{FanoutStats, MissingRefStatsAtomic, record_fanout_from_payload_records};

/// A relation with geometry resolved from way_index, ready for parallel processing.
/// Matches are resolved eagerly in `prepare_relation` while PBF borrows are alive,
/// avoiding cloning all relation tags to owned Strings.
pub(super) struct PreparedRelation {
    pub(super) osm_id: u64,
    pub(super) matches: SmallVec<[LayerMatch; 4]>,
    pub(super) member_ways: Vec<MemberWay>,
}

/// Estimate heap bytes for a single prepared relation (struct + member way coords).
fn estimate_prepared_rel_bytes(r: &PreparedRelation) -> usize {
    std::mem::size_of::<PreparedRelation>()
        + r.member_ways
            .iter()
            .map(|mw| 32 + mw.coords.len() * 16)
            .sum::<usize>()
}

/// Resolve relation geometry from way_index (serial I/O). Returns None if
/// the relation is not a multipolygon/boundary or has no resolvable member ways.
/// Way-index read failures are returned so corrupt data is not counted as a
/// missing relation reference.
/// Tag matching runs here while PBF borrows are alive, eliminating the need to
/// clone all relation tags to owned Strings.
#[hotpath::measure]
pub(super) fn prepare_relation(
    rel: &pbfhogg::Relation<'_>,
    way_index: &WayIndexReader,
    missing_ref_stats: &MissingRefStatsAtomic,
) -> Result<Option<PreparedRelation>, PipelineError> {
    let tags: SmallVec<[(&str, &str); 16]> = rel.tags().collect();
    let tag_helper = Tags(&tags);
    // Match while PBF borrows are alive; copy only relevant attribute values.
    let Some(matches) = shortbread::area_relation_matches(&tag_helper) else {
        return Ok(None);
    };

    let mut member_ways: Vec<MemberWay> = Vec::new();
    let mut had_missing_way_ref = false;

    for member in rel.members() {
        match member.id {
            MemberId::Way(way_id) => {
                let role = WayRole::from_str(member.role().unwrap_or(""));
                let way_id = WayId::new(way_id);
                let coords_e7 = way_index.get(way_id).map_err(|error| {
                    PipelineError::io(
                        format!(
                            "failed to read member way {} for relation {}",
                            way_id.as_i64(),
                            rel.id()
                        ),
                        error,
                    )
                })?;
                if let Some(coords_e7) = coords_e7 {
                    let merc: Vec<Point> = coords_e7
                        .iter()
                        .map(|coord| geometry::projection::project_e7(coord.lat, coord.lon))
                        .collect();
                    member_ways.push(MemberWay { role, coords: merc });
                } else {
                    had_missing_way_ref = true;
                    missing_ref_stats.record_relation_missing_way_ref();
                }
            }
            MemberId::Relation(_) => {
                missing_ref_stats.record_relation_non_way_member();
                missing_ref_stats.record_relation_nested_member();
            }
            MemberId::Node(_) => {
                missing_ref_stats.record_relation_non_way_member();
            }
            _ => {
                missing_ref_stats.record_relation_non_way_member();
            }
        }
    }
    if had_missing_way_ref {
        missing_ref_stats.record_relation_with_missing_way_refs();
    }

    if member_ways.is_empty() {
        return Ok(None);
    }

    #[allow(clippy::cast_sign_loss)]
    Ok(Some(PreparedRelation {
        osm_id: rel.id() as u64,
        matches,
        member_ways,
    }))
}

/// Per-worker accumulator for streaming relation outputs. Modeled on
/// the way phase's `WayAcc` (`phase12::ways`) - each rayon worker hands its sink to the shared
/// spill coalescer, eliminating the `Vec<Vec<SortRecord>>`
/// double-materialization without fragmenting the chunk directory.
pub(super) struct RelAcc {
    pub(super) sink: RecordSink,
    pub(super) bytes: usize,
    pub(super) count: u64,
    pub(super) point_emit: PointEmitScratch,
    pub(super) multipolygon_emit: MultipolygonEmitScratch,
    pub(super) fanout: FanoutStats,
}

impl RelAcc {
    fn new() -> Self {
        RelAcc {
            sink: RecordSink::new(),
            bytes: 0,
            count: 0,
            point_emit: PointEmitScratch::new(),
            multipolygon_emit: MultipolygonEmitScratch::new(),
            fanout: FanoutStats::new(),
        }
    }

    pub(super) fn flush(&mut self, spill: &sort::SpillCoalescer) {
        if self.sink.records.is_empty() {
            return;
        }
        spill.append(&self.sink.records, &self.sink.payload);
        self.count += self.sink.records.len() as u64;
        self.sink.clear_payload();
        self.bytes = 0;
    }
}

/// Totals from the streamed relation tail.
pub(super) struct RelationTail {
    pub(super) rel_count: u64,
    pub(super) features: u64,
    pub(super) max_inflight_bytes: usize,
}

/// Process a stream of relation blocks: prepare + emit, streaming outputs to
/// chunk files. One parallel pass over every relation with a single barrier at
/// the end. `par_bridge` pulls from the preparing iterator under its internal
/// lock, so block decode + `prepare_relation`'s way_index reads (cheap,
/// serial) interleave with relation processing on the worker threads instead
/// of alternating with it; per-batch flush barriers previously idled most of
/// the pool on each batch's slowest relation (giant coastal multipolygons:
/// P99 is ~100x P50). Blocks are consumed one at a time (prepared eagerly per
/// block, then dropped), so the source can be the in-RAM buffer or a filtered
/// PBF re-read without holding more than one block plus each worker's single
/// in-flight prepared relation.
#[hotpath::measure]
// too_many_lines is visible only without the hotpath feature, whose measure
// macro re-wraps the body.
// Panics: a poisoned lock or a failed sort push is unrecoverable; the Result
// carries relation-source and preparation errors.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::unwrap_in_result
)]
pub(super) fn process_relation_blocks(
    relation_blocks: impl Iterator<Item = Result<pbfhogg::PrimitiveBlock, PipelineError>> + Send,
    way_index: &WayIndexReader,
    missing_ref_stats: &MissingRefStatsAtomic,
    window: ZoomRange,
    sort_writer: &mut SortWriter,
    spill: &sort::SpillCoalescer,
    fanout_stats: &mut FanoutStats,
    fanout_caps: &[Option<std::num::NonZeroU32>],
    polygon_simplify_factor: f64,
) -> Result<RelationTail, PipelineError> {
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    // Per-worker sink flush threshold. The accumulators live for the WHOLE
    // tail, so a large threshold multiplies across workers - measured as a
    // 5.2 -> 12.1 GB peak-RSS regression on norway when the streamed tail
    // first flushed at the full sort budget. A flush is now a memcpy into
    // the shared coalescer (which handles chunk sizing), so small is cheap.
    const REL_ACC_FLUSH_BYTES: usize = 8 * 1024 * 1024;
    let flush_threshold = sort_writer.chunk_size_bytes().min(REL_ACC_FLUSH_BYTES);

    let rel_count = AtomicU64::new(0);
    let inflight_bytes = AtomicUsize::new(0);
    let max_inflight_bytes = AtomicUsize::new(0);
    let preparation_error = std::sync::Mutex::new(None::<PipelineError>);

    let result = relation_blocks
        .flat_map(|block| {
            let block = match block {
                Ok(block) => block,
                Err(error) => {
                    let mut slot = preparation_error
                        .lock()
                        .expect("relation preparation error lock");
                    if slot.is_none() {
                        *slot = Some(error);
                    }
                    return Vec::new().into_iter();
                }
            };
            // Prepare eagerly per block: PreparedRelation owns its data, so
            // the block can drop as soon as its relations are extracted. One
            // block's prepared set (~MBs) is the peak the producer side holds.
            let prepared: Vec<(PreparedRelation, usize)> = block
                .elements()
                .filter_map(|element| {
                    let pbfhogg::Element::Relation(rel) = element else {
                        return None;
                    };
                    rel_count.fetch_add(1, Ordering::Relaxed);
                    let prepared = match prepare_relation(&rel, way_index, missing_ref_stats) {
                        Ok(Some(prepared)) => prepared,
                        Ok(None) => return None,
                        Err(error) => {
                            let mut slot = preparation_error
                                .lock()
                                .expect("relation preparation error lock");
                            if slot.is_none() {
                                *slot = Some(error);
                            }
                            return None;
                        }
                    };
                    let bytes = estimate_prepared_rel_bytes(&prepared);
                    let now_inflight = inflight_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
                    max_inflight_bytes.fetch_max(now_inflight, Ordering::Relaxed);
                    Some((prepared, bytes))
                })
                .collect();
            prepared.into_iter()
        })
        .par_bridge()
        .fold(RelAcc::new, |mut acc, (rel, rel_bytes)| {
            let before = acc.sink.records.len();
            if let Err(error) = process_prepared_relation_into(
                rel,
                window,
                &mut acc.sink,
                &mut acc.point_emit,
                &mut acc.multipolygon_emit,
                fanout_caps,
                polygon_simplify_factor,
                &mut acc.fanout,
            ) {
                // Same first-error slot as preparation: the tail runs to its
                // barrier and returns the failure, never a partial success.
                let mut slot = preparation_error
                    .lock()
                    .expect("relation preparation error lock");
                if slot.is_none() {
                    *slot = Some(error.into());
                }
            }
            // Track fanout for this relation's records.
            record_fanout_from_payload_records(&acc.sink.records[before..], &mut acc.fanout);
            // Drop the geometry-scaled scratches after EVERY relation. The
            // fold accumulator lives for the whole tail (par_bridge keeps one
            // per worker), so warm scratch ratchets to the fattest relation a
            // worker ever processed - measured at NA scale as ~18 GB of
            // phase12 anon across the pool (24 workers x ~750 MB), evicting
            // the page cache the tail's own way_index mmap reads depend on
            // (2M major faults). Gated variants were tried and refuted on the
            // same day: a 1 MiB prepared-bytes gate left 14 GB and adding a
            // 2 MiB emitted-bytes gate left 12.6 GB, because the ratchet is
            // the aggregate of many sub-gate relations, not a few monsters.
            // The unconditional drop measured 5.95 GB peak phase12 anon at an
            // identical NA wall (288s): per-relation buffer regrowth is noise
            // next to the relation's own processing, while the engine-internal
            // reuse (the e2284ec de-churn win) lives WITHIN a single op and is
            // untouched by this.
            acc.multipolygon_emit = MultipolygonEmitScratch::new();
            inflight_bytes.fetch_sub(rel_bytes, Ordering::Relaxed);
            acc.bytes = acc.sink.bytes();
            if acc.bytes >= flush_threshold {
                acc.flush(spill);
                // The flush keeps capacity for the next fill; release it when
                // a monster emission ratcheted it far beyond the threshold.
                if acc.sink.payload.capacity() > 4 * flush_threshold {
                    acc.sink.payload.shrink_to(flush_threshold);
                    acc.sink.records.shrink_to(flush_threshold / 64);
                }
            }
            acc
        })
        // Finalize each fold accumulator on its worker the moment its batch
        // completes: flush the residual records to the coalescer and drop the
        // scratches. A finished accumulator queues for its reduce sibling
        // holding whatever capacity it ratcheted, and par_bridge produces
        // hundreds of batch accumulators - measured at NA as ~13 GB of
        // phase12 anon AFTER the per-relation monster gates landed. Reduce
        // then merges empty sinks: tally, count and fanout only.
        .map(|mut acc| {
            acc.flush(spill);
            acc.multipolygon_emit = MultipolygonEmitScratch::new();
            acc.point_emit = PointEmitScratch::new();
            acc.sink.payload = Vec::new();
            acc.sink.records = Vec::new();
            acc
        })
        .reduce(RelAcc::new, |mut a, mut b| {
            a.count += b.count;
            let base = a.sink.payload.len();
            a.sink.payload.append(&mut b.sink.payload);
            a.sink
                .records
                .extend(b.sink.records.drain(..).map(|r| sort::PayloadRef {
                    offset: r.offset + base,
                    ..r
                }));
            a.sink.tally.merge(&b.sink.tally);
            a.bytes += b.bytes;
            a.fanout.merge(&b.fanout);
            a
        });

    if let Some(error) = preparation_error
        .into_inner()
        .expect("relation preparation error lock poisoned")
    {
        return Err(error);
    }

    fanout_stats.merge(&result.fanout);
    sort_writer.merge_tally(&result.sink.tally);
    let mut count = result.count;
    // Push remaining records (below chunk_size threshold) through sort_writer's
    // normal buffering, so they merge with way records instead of creating
    // tiny standalone chunk files.
    #[allow(clippy::cast_possible_truncation)]
    {
        count += result.sink.records.len() as u64;
    }
    for record in result.sink.records {
        sort_writer
            .push_untracked(sort::SortRecord {
                key: record.key,
                data: record.bytes(&result.sink.payload).into(),
            })
            .expect("sort push failed");
    }
    Ok(RelationTail {
        rel_count: rel_count.into_inner(),
        features: count,
        max_inflight_bytes: max_inflight_bytes.into_inner(),
    })
}

/// Process a prepared relation's geometry into an external buffer (CPU-bound).
/// Called from rayon worker threads via `RelAcc` fold. Reuses the caller's
/// sink and emission scratch to avoid per-relation allocation.
#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(super) fn process_prepared_relation_into(
    rel: PreparedRelation,
    window: ZoomRange,
    sink: &mut RecordSink,
    point_emit: &mut PointEmitScratch,
    multipolygon_emit: &mut MultipolygonEmitScratch,
    fanout_caps: &[Option<std::num::NonZeroU32>],
    polygon_simplify_factor: f64,
    fanout: &mut FanoutStats,
) -> Result<(), RingCapError> {
    let multi = multipolygon::assemble_wrapped(&rel.member_ways);
    let shared_vertex_keys = relation_shared_vertex_keys(&rel.member_ways);

    if multi.polygons.is_empty() {
        return Ok(());
    }

    let mut matches = rel.matches;

    let total_area_m2: f64 = multi
        .polygons
        .iter()
        .map(|(outer, inners)| {
            geometry::polygon::area_sq_meters(outer)
                - inners
                    .iter()
                    .map(|inner| geometry::polygon::area_sq_meters(inner))
                    .sum::<f64>()
        })
        .sum();
    enrich_polygon_matches(&mut matches, total_area_m2);

    let ctx = EmitCtx {
        window,
        fanout_caps,
        polygon_simplify_factor,
    };
    for m in &matches {
        if let Some(strategy) = m.layer.polygon_point_strategy() {
            if let Some(label_point) = multipolygon::relation_label_point(&multi.polygons, strategy)
            {
                emit_match(
                    rel.osm_id,
                    FeatureGeometry::AreaLabel {
                        point: &label_point,
                    },
                    m,
                    &ctx,
                    sink,
                    point_emit,
                )?;
            }
            continue;
        }
        for (outer, inners) in &multi.polygons {
            // Assembled rings are open: preserve the final triangle vertex.
            if outer.len() < 3 {
                continue;
            }
            let mut outer = outer.clone();
            let mut inners = inners.clone();
            let _ = unwrap_antimeridian_path(&mut outer, false);
            for inner in &mut inners {
                let _ = unwrap_antimeridian_path(inner, false);
            }
            emit_match(
                rel.osm_id,
                FeatureGeometry::Area {
                    outer: &outer,
                    inners: &inners,
                    pins: &shared_vertex_keys,
                    polygon: multipolygon_emit,
                    fanout,
                },
                m,
                &ctx,
                sink,
                point_emit,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod source_error_tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn relation_source_failure_retains_class_and_cause() {
        let dir = tempfile::Builder::new()
            .prefix("relation-source-error-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .expect("project-local test directory");
        let way_index = crate::way_index::WayIndexWriter::create(dir.path())
            .expect("way writer")
            .finish_writing()
            .expect("empty way reader");
        let mut writer =
            SortWriter::new(&dir.path().join("sort"), 1024, sort::ChunkCompression::None)
                .expect("sort writer");
        let spill = sort::SpillCoalescer::new(writer.chunk_sink(), 1024);
        let source_error = PipelineError::failed("relation re-read: PBF read failed")
            .with_cause(std::io::Error::from_raw_os_error(5));
        let result = process_relation_blocks(
            std::iter::once(Err(source_error)),
            &way_index,
            &MissingRefStatsAtomic::default(),
            ZoomRange::new(0, 14).expect("zoom window"),
            &mut writer,
            &spill,
            &mut FanoutStats::new(),
            &[],
            1.0,
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("source failure must abort the relation tail"),
        };
        assert_eq!(error.kind(), crate::ErrorKind::Failed);
        assert_eq!(
            error
                .source()
                .expect("PBF source")
                .downcast_ref::<std::io::Error>()
                .expect("original I/O cause")
                .raw_os_error(),
            Some(5)
        );
    }
}
