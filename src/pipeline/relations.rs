use smallvec::SmallVec;

use crate::geometry::{self, Point, merc_bbox};
use crate::multipolygon::{self, MemberWay, WayRole};
use crate::shortbread::{self, GeomExpect, OsmGeomType, Tags};
use crate::shortbread::LayerMatch;
use crate::sort::{self, SortRecord, SortWriter};
use crate::way_index::WayIndex;
use pbfhogg::MemberId;

use super::stats::{
    DeferralStats, FanoutStats, MissingRefStatsAtomic,
    record_fanout_from_records,
};
use super::emit::{
    PointEmitScratch, LineEmitScratch, MultipolygonEmitScratch,
    emit_point_or_centroid, emit_line_feature, emit_multipolygon_feature,
    enrich_polygon_matches, unwrap_antimeridian_path, antimeridian_shifts_for_bbox,
    relation_shared_vertex_keys,
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

pub(super) const REL_BATCH_SIZE: usize = 1024;
pub(super) const REL_BATCH_BUDGET_DEFAULT: usize = 64 * 1024 * 1024; // 64 MB

/// Estimate heap bytes for a single prepared relation (struct + member way coords).
pub(super) fn estimate_prepared_rel_bytes(r: &PreparedRelation) -> usize {
    std::mem::size_of::<PreparedRelation>()
        + r.member_ways.iter().map(|mw| 32 + mw.coords.len() * 16).sum::<usize>()
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
    pub(super) records: Vec<SortRecord>,
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
    pub(super) fn flush(&mut self, chunk_dir: &std::path::Path, chunk_id: &std::sync::atomic::AtomicUsize) {
        if self.records.is_empty() {
            return;
        }
        let id = chunk_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = chunk_dir.join(format!("chunk_{id:04}.bin"));
        sort::write_sorted_chunk(&mut self.records, &path, self.compression)
            .expect("relation chunk write failed");
        self.chunk_paths.push(path);
        self.count += self.records.len() as u64;
        self.records.clear();
        self.bytes = 0;
    }
}

/// Process a batch of prepared relations in parallel, streaming outputs to chunk files.
#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
pub(super) fn flush_rel_batch(
    batch: Vec<PreparedRelation>,
    min_zoom: u8,
    max_zoom: u8,
    seam_reconcile_layers: &[u8],
    deferral_stats: &DeferralStats,
    sort_writer: &mut SortWriter,
    fanout_stats: &mut FanoutStats,
    fanout_caps: &[u32],
    polygon_simplify_factor: f64,
) -> u64 {
    use rayon::prelude::*;

    let chunk_id = std::sync::atomic::AtomicUsize::new(sort_writer.chunk_count());
    let chunk_dir = sort_writer.tmp_dir().to_path_buf();
    let chunk_size = sort_writer.chunk_size_bytes();
    let chunk_compression = sort_writer.compression();

    let result = batch
        .into_par_iter()
        .fold(
            || RelAcc {
                records: Vec::new(), bytes: 0, chunk_paths: Vec::new(), count: 0,
                point_emit: PointEmitScratch::new(),
                line_emit: LineEmitScratch::new(),
                multipolygon_emit: MultipolygonEmitScratch::new(),
                simp_scratch: geometry::SimplifyMultiScratch::new(),
                compression: chunk_compression,
                fanout: FanoutStats::new(),
            },
            |mut acc, rel| {
                let before = acc.records.len();
                process_prepared_relation_into(
                    rel, min_zoom, max_zoom, seam_reconcile_layers, deferral_stats,
                    &mut acc.records,
                    &mut acc.point_emit,
                    &mut acc.line_emit,
                    &mut acc.multipolygon_emit,
                    &mut acc.simp_scratch,
                    fanout_caps,
                    polygon_simplify_factor,
                );
                // Track fanout for this relation's records.
                record_fanout_from_records(&acc.records[before..], &mut acc.fanout);
                // Harvest cap events from multipolygon emit scratch.
                for &(idx, tiles, oid) in &acc.multipolygon_emit.cap_events {
                    let layer = idx as usize / 15;
                    let zoom = idx as usize % 15;
                    acc.fanout.record_cap(layer, zoom, tiles, oid);
                }
                for r in &acc.records[before..] {
                    acc.bytes += r.data.len() + std::mem::size_of::<SortRecord>();
                }
                if acc.bytes >= chunk_size {
                    acc.flush(&chunk_dir, &chunk_id);
                }
                acc
            },
        )
        // Don't flush in .map() - collect remaining records back for sort_writer
        // to avoid creating many tiny chunk files (one per rayon accumulator).
        .reduce(
            || RelAcc {
                records: Vec::new(), bytes: 0, chunk_paths: Vec::new(), count: 0,
                point_emit: PointEmitScratch::new(),
                line_emit: LineEmitScratch::new(),
                multipolygon_emit: MultipolygonEmitScratch::new(),
                simp_scratch: geometry::SimplifyMultiScratch::new(),
                compression: chunk_compression,
                fanout: FanoutStats::new(),
            },
            |mut a, mut b| {
                a.chunk_paths.extend(b.chunk_paths);
                a.count += b.count;
                a.records.append(&mut b.records);
                a.bytes += b.bytes;
                a.fanout.merge(&b.fanout);
                a
            },
        );

    fanout_stats.merge(&result.fanout);
    sort_writer.adopt_chunk_files(result.chunk_paths);
    let mut count = result.count;
    // Push remaining records (below chunk_size threshold) through sort_writer's
    // normal buffering, so they merge with way records instead of creating
    // tiny standalone chunk files.
    #[allow(clippy::cast_possible_truncation)]
    {
        count += result.records.len() as u64;
    }
    for record in result.records {
        sort_writer.push(record).expect("sort push failed");
    }
    count
}

/// Process a prepared relation's geometry into an external buffer (CPU-bound).
/// Called from rayon worker threads via `RelAcc` fold. Reuses the caller's
/// `records` vec and `simp_scratch` to avoid per-relation allocation.
#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(super) fn process_prepared_relation_into(
    rel: PreparedRelation,
    min_zoom: u8,
    max_zoom: u8,
    seam_reconcile_layers: &[u8],
    deferral_stats: &DeferralStats,
    records: &mut Vec<SortRecord>,
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

    let total_area_m2: f64 = multi.polygons.iter()
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
                                z_lo, z_hi, records, multipolygon_emit, simp_scratch,
                                sr, Some(deferral_stats), fc, polygon_simplify_factor,
                            );
                        } else {
                            let outer_shifted: Vec<Point> = outer_unwrapped
                                .iter()
                                .map(|p| Point { x: p.x + shift, y: p.y })
                                .collect();
                            let inners_shifted: Vec<Vec<Point>> = inners_unwrapped
                                .iter()
                                .map(|ring| {
                                    ring.iter()
                                        .map(|p| Point { x: p.x + shift, y: p.y })
                                        .collect()
                                })
                                .collect();
                            emit_multipolygon_feature(
                                rel.osm_id,
                                &outer_shifted,
                                &inners_shifted,
                                Some(&shared_vertex_keys),
                                m,
                                z_lo, z_hi, records, multipolygon_emit, simp_scratch,
                                sr, Some(deferral_stats), fc, polygon_simplify_factor,
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
                        records,
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
                                records,
                                line_emit,
                            );
                        } else {
                            let shifted: Vec<Point> = coords
                                .iter()
                                .map(|p| Point { x: p.x + shift, y: p.y })
                                .collect();
                            emit_line_feature(
                                rel.osm_id,
                                &shifted,
                                &[],
                                m,
                                z_lo,
                                z_hi,
                                records,
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
