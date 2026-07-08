use smallvec::SmallVec;

use crate::geometry::{self, Point, merc_bbox};
use crate::multipolygon::{self, MemberWay, WayRole};
use crate::shortbread::LayerMatch;
use crate::shortbread::{self, GeomExpect, OsmGeomType, Tags};
use crate::sort::{self, SortWriter};
use crate::way_index::WayIndex;
use pbfhogg::MemberId;

use super::emit::{
    LineEmitScratch, MultipolygonEmitScratch, PointEmitScratch, RecordSink,
    antimeridian_shifts_for_bbox, emit_line_feature, emit_multipolygon_feature,
    emit_point_or_centroid, enrich_polygon_matches, relation_shared_vertex_keys,
    unwrap_antimeridian_path,
};
use super::stats::{
    DeferralStats, FanoutStats, MissingRefStatsAtomic, record_fanout_from_payload_records,
};

/// A relation with geometry resolved from way_index, ready for parallel processing.
/// Matches are resolved eagerly in `prepare_relation` while PBF borrows are alive,
/// avoiding cloning all relation tags to owned Strings.
pub(super) struct PreparedRelation {
    pub(super) osm_id: u64,
    pub(super) matches: SmallVec<[LayerMatch; 4]>,
    pub(super) member_ways: Vec<MemberWay>,
    pub(super) is_boundary: bool,
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
/// Tag matching runs here while PBF borrows are alive, eliminating the need to
/// clone all relation tags to owned Strings.
#[hotpath::measure]
pub(super) fn prepare_relation(
    rel: &pbfhogg::Relation<'_>,
    way_index: &WayIndex,
    missing_ref_stats: &MissingRefStatsAtomic,
) -> Option<PreparedRelation> {
    // Fast reject without tag allocation: most relations are not multipolygon/boundary.
    let mut rel_type = "";
    for (k, v) in rel.tags() {
        if k == "type" {
            rel_type = v;
            break;
        }
    }
    if rel_type != "multipolygon" && rel_type != "boundary" {
        return None;
    }
    let tags: SmallVec<[(&str, &str); 16]> = rel.tags().collect();
    let tag_helper = Tags(&tags);

    // Match while PBF borrows are alive - attrs copy only the relevant tag values
    // into Cow::Owned, avoiding cloning ALL tags to String.
    let matches = shortbread::match_element(&tag_helper, OsmGeomType::MultiPolygon);
    if matches.is_empty() {
        return None;
    }

    let is_boundary = tag_helper.has_value("boundary", "administrative");

    let mut member_ways: Vec<MemberWay> = Vec::new();
    let mut had_missing_way_ref = false;

    for member in rel.members() {
        match member.id {
            MemberId::Way(way_id) => {
                let role = WayRole::from_str(member.role().unwrap_or(""));
                if let Some(coords_e7) = way_index.get(way_id) {
                    let merc: Vec<Point> = coords_e7
                        .iter()
                        .map(|&(lat, lon)| geometry::project_e7(lat, lon))
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
        return None;
    }

    #[allow(clippy::cast_sign_loss)]
    Some(PreparedRelation {
        osm_id: rel.id() as u64,
        matches,
        member_ways,
        is_boundary,
    })
}

/// Per-worker accumulator for streaming relation outputs to chunk files.
/// Modeled on `OceanAcc` in ocean.rs - each rayon worker flushes directly
/// to disk, eliminating the `Vec<Vec<SortRecord>>` double-materialization.
pub(super) struct RelAcc {
    pub(super) sink: RecordSink,
    pub(super) bytes: usize,
    pub(super) chunk_paths: Vec<std::path::PathBuf>,
    pub(super) count: u64,
    pub(super) point_emit: PointEmitScratch,
    pub(super) line_emit: LineEmitScratch,
    pub(super) multipolygon_emit: MultipolygonEmitScratch,
    pub(super) simp_scratch: geometry::SimplifyMultiScratch,
    pub(super) compression: sort::ChunkCompression,
    pub(super) fanout: FanoutStats,
}

impl RelAcc {
    fn new(compression: sort::ChunkCompression) -> Self {
        RelAcc {
            sink: RecordSink::new(),
            bytes: 0,
            chunk_paths: Vec::new(),
            count: 0,
            point_emit: PointEmitScratch::new(),
            line_emit: LineEmitScratch::new(),
            multipolygon_emit: MultipolygonEmitScratch::new(),
            simp_scratch: geometry::SimplifyMultiScratch::new(),
            compression,
            fanout: FanoutStats::new(),
        }
    }

    pub(super) fn flush(
        &mut self,
        chunk_dir: &std::path::Path,
        chunk_id: &std::sync::atomic::AtomicUsize,
    ) {
        if self.sink.records.is_empty() {
            return;
        }
        let paths = sort::write_partitioned_payload_chunks(
            &mut self.sink.records,
            &self.sink.payload,
            chunk_dir,
            chunk_id,
            self.compression,
        )
        .expect("relation chunk write failed");
        self.chunk_paths.extend(paths);
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

/// Process all buffered relation blocks: prepare + emit, streaming outputs to
/// chunk files. One parallel pass over every relation with a single barrier at
/// the end. `par_bridge` pulls from the preparing iterator under its internal
/// lock, so `prepare_relation`'s way_index reads (cheap, serial) interleave
/// with relation processing on the worker threads instead of alternating with
/// it; per-batch flush barriers previously idled most of the pool on each
/// batch's slowest relation (giant coastal multipolygons: P99 is ~100x P50).
/// In-flight prepared memory is bounded by construction: each worker holds at
/// most one prepared relation at a time (no batch accumulation).
#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
pub(super) fn process_relation_blocks(
    relation_blocks: &[pbfhogg::PrimitiveBlock],
    way_index: &WayIndex,
    missing_ref_stats: &MissingRefStatsAtomic,
    min_zoom: u8,
    max_zoom: u8,
    seam_reconcile_layers: &[u8],
    deferral_stats: &DeferralStats,
    sort_writer: &mut SortWriter,
    fanout_stats: &mut FanoutStats,
    fanout_caps: &[u32],
    polygon_simplify_factor: f64,
) -> RelationTail {
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    let chunk_id = AtomicUsize::new(sort_writer.chunk_count());
    let chunk_dir = sort_writer.tmp_dir().to_path_buf();
    // Per-worker sink flush threshold. The accumulators live for the WHOLE
    // tail (unlike the way path, where an acc is scoped to one block task),
    // so flushing at the full sort-chunk budget would let every rayon worker
    // buffer up to that budget simultaneously - measured as a 5.2 -> 12.1 GB
    // peak-RSS regression on norway when this streamed tail first landed.
    // Cap each worker's buffered records well below the chunk budget; the
    // resulting chunk files are smaller but still far above the merge
    // fan-in's comfort zone.
    const REL_ACC_FLUSH_BYTES: usize = 32 * 1024 * 1024;
    let flush_threshold = sort_writer.chunk_size_bytes().min(REL_ACC_FLUSH_BYTES);
    let chunk_compression = sort_writer.compression();

    let rel_count = AtomicU64::new(0);
    let inflight_bytes = AtomicUsize::new(0);
    let max_inflight_bytes = AtomicUsize::new(0);

    let result = relation_blocks
        .iter()
        .flat_map(pbfhogg::PrimitiveBlock::elements)
        .filter_map(|element| {
            let pbfhogg::Element::Relation(rel) = element else {
                return None;
            };
            rel_count.fetch_add(1, Ordering::Relaxed);
            let prepared = prepare_relation(&rel, way_index, missing_ref_stats)?;
            let bytes = estimate_prepared_rel_bytes(&prepared);
            let now_inflight = inflight_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
            max_inflight_bytes.fetch_max(now_inflight, Ordering::Relaxed);
            Some((prepared, bytes))
        })
        .par_bridge()
        .fold(
            || RelAcc::new(chunk_compression),
            |mut acc, (rel, rel_bytes)| {
                let before = acc.sink.records.len();
                process_prepared_relation_into(
                    rel,
                    min_zoom,
                    max_zoom,
                    seam_reconcile_layers,
                    deferral_stats,
                    &mut acc.sink,
                    &mut acc.point_emit,
                    &mut acc.line_emit,
                    &mut acc.multipolygon_emit,
                    &mut acc.simp_scratch,
                    fanout_caps,
                    polygon_simplify_factor,
                );
                // Track fanout for this relation's records.
                record_fanout_from_payload_records(&acc.sink.records[before..], &mut acc.fanout);
                // Harvest cap events from multipolygon emit scratch.
                // `emit_multipolygon_feature` only clears cap_events when it runs,
                // so a relation that emits only points/lines would re-harvest the
                // previous relation's events across this fold accumulator. Clear
                // after harvesting to count each cap event exactly once.
                for &(idx, tiles, oid) in &acc.multipolygon_emit.cap_events {
                    let layer = idx as usize / 15;
                    let zoom = idx as usize % 15;
                    acc.fanout.record_cap(layer, zoom, tiles, oid);
                }
                acc.multipolygon_emit.cap_events.clear();
                inflight_bytes.fetch_sub(rel_bytes, Ordering::Relaxed);
                acc.bytes = acc.sink.bytes();
                if acc.bytes >= flush_threshold {
                    acc.flush(&chunk_dir, &chunk_id);
                }
                acc
            },
        )
        // Don't flush in reduce - collect remaining records back for sort_writer
        // to avoid creating many tiny chunk files (one per rayon accumulator).
        .reduce(
            || RelAcc::new(chunk_compression),
            |mut a, mut b| {
                a.chunk_paths.extend(b.chunk_paths);
                a.count += b.count;
                let base = a.sink.payload.len();
                a.sink.payload.append(&mut b.sink.payload);
                a.sink.records.extend(
                    b.sink
                        .records
                        .drain(..)
                        .map(|(key, off, len)| (key, off + base, len)),
                );
                a.sink.tally.merge(&b.sink.tally);
                a.bytes += b.bytes;
                a.fanout.merge(&b.fanout);
                a
            },
        );

    fanout_stats.merge(&result.fanout);
    sort_writer.adopt_chunk_files(result.chunk_paths);
    sort_writer.merge_tally(&result.sink.tally);
    let mut count = result.count;
    // Push remaining records (below chunk_size threshold) through sort_writer's
    // normal buffering, so they merge with way records instead of creating
    // tiny standalone chunk files.
    #[allow(clippy::cast_possible_truncation)]
    {
        count += result.sink.records.len() as u64;
    }
    for (key, off, len) in result.sink.records {
        sort_writer
            .push_untracked(sort::SortRecord {
                key,
                data: result.sink.payload[off..off + len].into(),
            })
            .expect("sort push failed");
    }
    RelationTail {
        rel_count: rel_count.into_inner(),
        features: count,
        max_inflight_bytes: max_inflight_bytes.into_inner(),
    }
}

/// Process a prepared relation's geometry into an external buffer (CPU-bound).
/// Called from rayon worker threads via `RelAcc` fold. Reuses the caller's
/// `sink` and `simp_scratch` to avoid per-relation allocation.
#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(super) fn process_prepared_relation_into(
    rel: PreparedRelation,
    min_zoom: u8,
    max_zoom: u8,
    seam_reconcile_layers: &[u8],
    deferral_stats: &DeferralStats,
    sink: &mut RecordSink,
    point_emit: &mut PointEmitScratch,
    line_emit: &mut LineEmitScratch,
    multipolygon_emit: &mut MultipolygonEmitScratch,
    simp_scratch: &mut geometry::SimplifyMultiScratch,
    fanout_caps: &[u32],
    polygon_simplify_factor: f64,
) {
    let multi = multipolygon::assemble(&rel.member_ways);
    let shared_vertex_keys = relation_shared_vertex_keys(&rel.member_ways);

    if multi.polygons.is_empty() {
        return;
    }

    let mut matches = rel.matches;

    let total_area_m2: f64 = multi
        .polygons
        .iter()
        .map(|(outer, _)| geometry::area_sq_meters(outer))
        .sum();
    enrich_polygon_matches(&mut matches, total_area_m2);

    for m in &matches {
        let z_lo = m.min_zoom.max(min_zoom);
        let z_hi = m.max_zoom.min(max_zoom);
        if z_lo > z_hi {
            continue;
        }

        match m.geom_expect {
            GeomExpect::Polygon => {
                for (outer, inners) in &multi.polygons {
                    if outer.len() < 4 {
                        continue;
                    }
                    let mut outer_unwrapped = outer.clone();
                    let mut inners_unwrapped = inners.clone();
                    let _ = unwrap_antimeridian_path(&mut outer_unwrapped, true);
                    for inner in &mut inners_unwrapped {
                        let _ = unwrap_antimeridian_path(inner, true);
                    }
                    let bbox = merc_bbox(&outer_unwrapped);
                    let sr = seam_reconcile_layers[m.layer as usize];
                    let fc = fanout_caps.get(m.layer as usize).copied().unwrap_or(0);
                    for shift in antimeridian_shifts_for_bbox(&bbox) {
                        if shift == 0.0 {
                            emit_multipolygon_feature(
                                rel.osm_id,
                                &outer_unwrapped,
                                &inners_unwrapped,
                                Some(&shared_vertex_keys),
                                m,
                                z_lo,
                                z_hi,
                                sink,
                                multipolygon_emit,
                                simp_scratch,
                                sr,
                                Some(deferral_stats),
                                fc,
                                polygon_simplify_factor,
                            );
                        } else {
                            let outer_shifted: Vec<Point> = outer_unwrapped
                                .iter()
                                .map(|p| Point {
                                    x: p.x + shift,
                                    y: p.y,
                                })
                                .collect();
                            let inners_shifted: Vec<Vec<Point>> = inners_unwrapped
                                .iter()
                                .map(|ring| {
                                    ring.iter()
                                        .map(|p| Point {
                                            x: p.x + shift,
                                            y: p.y,
                                        })
                                        .collect()
                                })
                                .collect();
                            emit_multipolygon_feature(
                                rel.osm_id,
                                &outer_shifted,
                                &inners_shifted,
                                Some(&shared_vertex_keys),
                                m,
                                z_lo,
                                z_hi,
                                sink,
                                multipolygon_emit,
                                simp_scratch,
                                sr,
                                Some(deferral_stats),
                                fc,
                                polygon_simplify_factor,
                            );
                        }
                    }
                }
            }
            GeomExpect::PolygonCentroid | GeomExpect::PolygonPointOnSurface => {
                for (outer, inners) in &multi.polygons {
                    if outer.len() < 4 {
                        continue;
                    }
                    let mut outer_unwrapped = outer.clone();
                    let mut inners_unwrapped = inners.clone();
                    let _ = unwrap_antimeridian_path(&mut outer_unwrapped, true);
                    for inner in &mut inners_unwrapped {
                        let _ = unwrap_antimeridian_path(inner, true);
                    }
                    let bbox = merc_bbox(&outer_unwrapped);
                    emit_point_or_centroid(
                        rel.osm_id,
                        &outer_unwrapped,
                        Some(&inners_unwrapped),
                        &bbox,
                        m,
                        z_lo,
                        z_hi,
                        sink,
                        point_emit,
                    );
                }
            }
            GeomExpect::Line => {
                if !rel.is_boundary {
                    continue;
                }
                for mw in &rel.member_ways {
                    if mw.coords.len() < 2 {
                        continue;
                    }
                    let mut coords = mw.coords.clone();
                    let _ = unwrap_antimeridian_path(&mut coords, false);
                    let bbox = merc_bbox(&coords);
                    for shift in antimeridian_shifts_for_bbox(&bbox) {
                        if shift == 0.0 {
                            emit_line_feature(
                                rel.osm_id,
                                &coords,
                                &[],
                                m,
                                z_lo,
                                z_hi,
                                sink,
                                line_emit,
                            );
                        } else {
                            let shifted: Vec<Point> = coords
                                .iter()
                                .map(|p| Point {
                                    x: p.x + shift,
                                    y: p.y,
                                })
                                .collect();
                            emit_line_feature(
                                rel.osm_id,
                                &shifted,
                                &[],
                                m,
                                z_lo,
                                z_hi,
                                sink,
                                line_emit,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
