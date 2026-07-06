use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::geometry::{self, MercBbox, Point, merc_bbox};
use crate::mvt::{self, GeomType};
use crate::node_index::{NodeIndex, NodeStore, NodeStoreReader, SortedNodeStore};
use crate::pmtiles_writer;
use crate::shortbread::{self, GeomExpect, OsmGeomType, Tags};
use crate::sort::{SortRecord, SortWriter};
use crate::way_index::WayIndex;
use crate::wire_format::encode_attrs_bytes;
use pbfhogg::{BlobFilter, BlockType, Element, ElementReader, PrimitiveBlock};

use super::emit::{
    LineEmitScratch, PointEmitScratch, PolygonEmitScratch, antimeridian_shifts_for_bbox,
    emit_line_feature, emit_point_or_centroid, emit_polygon_feature, enrich_polygon_matches,
    push_sort_record, unwrap_antimeridian_path,
};
use super::relations::{
    PreparedRelation, REL_BATCH_BUDGET_DEFAULT, REL_BATCH_SIZE, estimate_prepared_rel_bytes,
    flush_rel_batch, prepare_relation,
};
use super::stats::{
    DeferralStats, FanoutStats, MissingRefStatsAtomic, Phase12Stats, record_fanout_from_records,
};
use super::{PipelineError, SORT_CHUNKS_DIR, TilegenConfig, current_rss_kb};

pub(super) const LON_E7_FULL_CIRCLE: i64 = 3_600_000_000;
/// Default memory budget per sort chunk (1 GB).
pub(super) const DEFAULT_SORT_CHUNK_SIZE: usize = 1 << 30;
/// Default way in-flight budget for the standard node-store path.
pub(super) const DEFAULT_WAY_BUDGET: usize = 128 * 1024 * 1024; // 128 MB
/// Default way in-flight budget for locations-on-ways mode.
pub(super) const DEFAULT_WAY_BUDGET_LOCATIONS: usize = 256 * 1024 * 1024; // 256 MB
/// Reject unsorted flat-index path above this input size unless explicitly overridden.
pub(super) const MAX_FLAT_PBF_SIZE: u64 = 1024 * 1024 * 1024; // 1 GB

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NodeStoreMode {
    None,
    Sorted,
    Flat { unsafe_override: bool },
}

pub(super) fn unsorted_flat_guard_error(pbf_size: u64) -> PipelineError {
    PipelineError(format!(
        "PBF file is {:.1} GB but does not declare Sort.Type_then_ID.\n\
         The flat node index would create a ~96 GB sparse file, causing severe\n\
         performance degradation on machines with <128 GB RAM. Options:\n\
         1. Use --force-sorted if the PBF is actually sorted (most Geofabrik extracts are)\n\
         2. Sort the PBF first with: pbfhogg sort input.pbf -o sorted.pbf\n\
         3. Alternative sorter: osmium sort input.pbf -o sorted.pbf\n\
         4. Use a sorted PBF from Geofabrik or planet.openstreetmap.org",
        pbf_size as f64 / (1024.0 * 1024.0 * 1024.0),
    ))
}

pub(super) fn select_node_store_mode(
    locations_on_ways: bool,
    header_sorted: bool,
    force_sorted: bool,
    pbf_size: u64,
    allow_unsafe_flat_index: bool,
) -> Result<NodeStoreMode, PipelineError> {
    if locations_on_ways {
        return Ok(NodeStoreMode::None);
    }
    if header_sorted || force_sorted {
        return Ok(NodeStoreMode::Sorted);
    }
    if pbf_size > MAX_FLAT_PBF_SIZE && !allow_unsafe_flat_index {
        return Err(unsorted_flat_guard_error(pbf_size));
    }
    Ok(NodeStoreMode::Flat {
        unsafe_override: allow_unsafe_flat_index,
    })
}

#[hotpath::measure]
#[allow(clippy::too_many_lines)]
#[allow(clippy::unwrap_in_result)]
pub(super) fn phase_read_and_process(
    config: &TilegenConfig,
) -> Result<(SortWriter, MercBbox, Phase12Stats), PipelineError> {
    eprintln!("\n--- Phase 1+2: Reading PBF + processing features ---");

    let sort_chunk_budget = if config.sort_chunk_size > 0 {
        config.sort_chunk_size
    } else {
        DEFAULT_SORT_CHUNK_SIZE
    };

    // Option so we can move to drain thread during way phase and get back after.
    let mut sort_writer: Option<SortWriter> = Some(SortWriter::new(
        &config.tmp_dir.join(SORT_CHUNKS_DIR),
        sort_chunk_budget,
        config.compress_sort_chunks,
    )?);

    // Decode threads: give 1/3 of budget to pbfhogg decode, rest to rayon processing.
    let decode_threads = (config.threads / 3).max(1);
    let reader = ElementReader::from_path(&config.pbf_path)
        .map_err(|e| PipelineError(format!("failed to open PBF: {e}")))?
        .decode_threads(decode_threads);

    let idx_dir = &config.tmp_dir;
    // Option so we can consume it via .take() on first Way element.
    let locations_on_ways = config.locations_on_ways
        || reader
            .header()
            .optional_features()
            .iter()
            .any(|f| f == "LocationsOnWays");

    let pbf_size = std::fs::metadata(&config.pbf_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let node_store_mode = select_node_store_mode(
        locations_on_ways,
        reader.header().is_sorted(),
        config.force_sorted,
        pbf_size,
        config.allow_unsafe_flat_index,
    )?;
    let mut node_store_opt: Option<NodeStore> = match node_store_mode {
        NodeStoreMode::None => {
            eprintln!("  LocationsOnWays - skipping node store");
            None
        }
        NodeStoreMode::Sorted => {
            if config.force_sorted && !reader.header().is_sorted() {
                eprintln!("  --force-sorted: assuming sorted PBF (will abort if not)");
            } else {
                eprintln!("  PBF declares Sort.Type_then_ID - using compact node store");
            }
            Some(NodeStore::Sorted(SortedNodeStore::new()))
        }
        NodeStoreMode::Flat { unsafe_override } => {
            if unsafe_override {
                eprintln!(
                    "  WARNING: unsafe flat index override enabled; bypassing unsorted-size and flat-index-size safety guardrails"
                );
            }
            eprintln!("  PBF not sorted - using flat mmap node index");
            Some(NodeStore::Flat(if unsafe_override {
                NodeIndex::create_unbounded(&idx_dir.join("nodes.idx"))?
            } else {
                NodeIndex::create(&idx_dir.join("nodes.idx"))?
            }))
        }
    };
    // Option so we can move to drain thread during way phase and get back after.
    let mut way_index: Option<WayIndex> = Some(WayIndex::create(idx_dir)?);

    let mut node_count: u64 = 0;
    let mut way_count: u64 = 0;
    let mut rel_count: u64 = 0;
    let mut features_emitted: u64 = 0;
    let mut node_store_stats: Option<(u64, usize)> = None;
    let mut fanout_stats = FanoutStats::new();
    let missing_ref_stats = std::sync::Arc::new(MissingRefStatsAtomic::default());
    let deferral_stats = std::sync::Arc::new(DeferralStats::new());
    // Track data extent for ocean shapefile filtering
    let mut min_lat_e7: i32 = i32::MAX;
    let mut max_lat_e7: i32 = i32::MIN;
    let mut min_lon_e7: i32 = i32::MAX;
    let mut max_lon_e7: i32 = i32::MIN;
    let mut min_lon_shifted_e7: i64 = i64::MAX;
    let mut max_lon_shifted_e7: i64 = i64::MIN;

    let min_z = config.min_zoom;
    let max_z = config.max_zoom;

    let mut rel_batch: Vec<PreparedRelation> = Vec::with_capacity(REL_BATCH_SIZE);
    let rel_budget = if config.rel_batch_budget > 0 {
        config.rel_batch_budget
    } else {
        REL_BATCH_BUDGET_DEFAULT
    };

    // Block-level dispatch: worker thread receives entire PrimitiveBlocks containing
    // ways, extracts RawWay data and processes via rayon. Main thread sends blocks
    // and drains results - no per-way work on the main thread during the way phase.
    let mut block_tx: Option<std::sync::mpsc::SyncSender<PrimitiveBlock>> = None;
    let mut worker_handle: Option<std::thread::JoinHandle<()>> = None;
    // Drain thread owns way_index + sort_writer during way phase, returns them when done.
    let mut drain_handle: Option<
        std::thread::JoinHandle<(WayIndex, SortWriter, u64, FanoutStats)>,
    > = None;

    // Buffer relation blocks - processed after all PBF blocks are consumed so that
    // late way blocks (common in locations-on-ways PBFs) don't hit a finalized way_index.
    let mut relation_blocks: Vec<PrimitiveBlock> = Vec::new();

    // High-water-mark counters for in-flight memory tracking.
    let way_hwm = std::sync::Arc::new(AtomicUsize::new(0));
    let mut rel_batch_bytes: usize = 0;
    let mut max_rel_batch_bytes: usize = 0;

    // Reusable buffer hoisted out of the PBF closure to avoid per-element
    // allocation (~200M allocs at planet scale). Cleared each iteration.
    // tags_vec cannot be hoisted: it holds &str references into PBF elements
    // that don't outlive the closure body (mutable reference invariance).
    // Global shared-node prepass: detect junction nodes across PBF blocks.
    // Runs on its own thread, overlapping the node phase: the prepass reads
    // only way blobs (BlobFilter::only_ways) while the main read below is
    // still consuming node blobs, so the two scan disjoint file sections.
    // The result is not needed until the first way block arrives - joined
    // there. If the prepass outlives the node phase, the join blocks and the
    // overlap is partial; the produced set is identical either way.
    let prepass_pbf_path = config.pbf_path.clone();
    let mut prepass_handle: Option<std::thread::JoinHandle<Result<FxHashSet<i64>, PipelineError>>> =
        Some(std::thread::spawn(move || {
            prepass_shared_nodes(&prepass_pbf_path, decode_threads)
        }));

    let mut node_records: Vec<SortRecord> = Vec::new();

    // Macro to handle Node and DenseNode identically - both types expose the
    // same API (.id(), .decimicro_lat(), .decimicro_lon(), .tags()) but are
    // distinct types, so a generic function would not work without a trait.
    macro_rules! handle_node {
        ($node:expr) => {{
            node_count += 1;
            let lat_e7 = $node.decimicro_lat();
            let lon_e7 = $node.decimicro_lon();
            if let Some(ns) = node_store_opt.as_mut() {
                ns.put($node.id(), lat_e7, lon_e7);
            }

            min_lat_e7 = min_lat_e7.min(lat_e7);
            max_lat_e7 = max_lat_e7.max(lat_e7);
            min_lon_e7 = min_lon_e7.min(lon_e7);
            max_lon_e7 = max_lon_e7.max(lon_e7);
            let shifted_lon_e7 = lon_e7_shifted_360(lon_e7);
            min_lon_shifted_e7 = min_lon_shifted_e7.min(shifted_lon_e7);
            max_lon_shifted_e7 = max_lon_shifted_e7.max(shifted_lon_e7);

            if $node.tags().next().is_some() {
                let tags_vec: Vec<(&str, &str)> = $node.tags().collect();
                node_records.clear();
                #[allow(clippy::cast_sign_loss)]
                let n = process_node(
                    $node.id() as u64, lat_e7, lon_e7,
                    &tags_vec, min_z, max_z, &mut node_records,
                );
                // Panic: inside PBF callback - can't propagate Result. Disk I/O failure is unrecoverable.
                for r in node_records.drain(..) {
                    sort_writer.as_mut().expect("sort_writer taken by drain thread")
                        .push(r).expect("sort push failed");
                }
                features_emitted += n;
            }
        }};
    }

    for block_result in reader.into_blocks_pipelined() {
        let block = block_result.map_err(|e| PipelineError(format!("PBF read failed: {e}")))?;

        // Classify block by reading first wire tag byte per group -
        // no element decoding. Sorted PBFs have single-type blocks.
        match block.block_type() {
            BlockType::DenseNodes | BlockType::Nodes => {
                // Node block - process inline
                block.for_each_element(|element| match element {
                    Element::DenseNode(node) => handle_node!(node),
                    Element::Node(node) => handle_node!(node),
                    _ => {}
                });
            }
            BlockType::Ways => {
                // Way block - send entire block to worker thread.
                // Count ways from block (elements() re-parses from bytes, cheap).
                way_count += block
                    .elements()
                    .filter(|e| matches!(e, Element::Way(_)))
                    .count() as u64;

                // Spawn worker + drain threads on first way block
                if block_tx.is_none() {
                    let nr: Option<std::sync::Arc<NodeStoreReader>> = if locations_on_ways {
                        None
                    } else {
                        let ns = node_store_opt.take().expect("node store already consumed");
                        let r = std::sync::Arc::new(
                            ns.into_reader()
                                .expect("failed to convert node store to reader"),
                        );
                        node_store_stats = r.sorted_stats();
                        Some(r)
                    };
                    if locations_on_ways {
                        eprintln!("  LocationsOnWays mode - processing ways (no node store)...");
                    } else {
                        eprintln!(
                            "  Node store finalized ({node_count} nodes), processing ways..."
                        );
                    }

                    let (btx, brx) = std::sync::mpsc::sync_channel::<PrimitiveBlock>(1);
                    // Capacity must be >= MAX_INFLIGHT: rayon tasks block on send()
                    // while holding a rayon thread. If capacity < inflight tasks,
                    // blocked senders tie up all rayon threads → worker (which runs
                    // inside rayon::in_place_scope) can't make progress → deadlock.
                    let (rtx, rrx) =
                        std::sync::mpsc::sync_channel::<Vec<ProcessedWay>>(MAX_INFLIGHT);
                    let nr_clone = nr.clone();
                    let way_hwm_clone = std::sync::Arc::clone(&way_hwm);
                    let missing_ref_stats_clone = std::sync::Arc::clone(&missing_ref_stats);
                    let deferral_stats_clone = std::sync::Arc::clone(&deferral_stats);
                    let mz = min_z;
                    let xz = max_z;
                    let srl = config.seam_reconcile_layers;
                    let fcs = config.fanout_caps;
                    let psf = config.polygon_simplify_factor;
                    // First point where the shared-node set is needed: join
                    // the prepass thread spawned before the node phase.
                    let gsn: std::sync::Arc<FxHashSet<i64>> = std::sync::Arc::new(
                        prepass_handle
                            .take()
                            .expect("prepass joined twice")
                            .join()
                            .map_err(|_| {
                                PipelineError("shared-node prepass thread panicked".to_string())
                            })??,
                    );
                    // Multi-block overlap: rayon::scope allows multiple blocks' ways
                    // in the pool simultaneously. Byte-budgeted in-flight control
                    // limits total estimated memory, with a count ceiling as safety net.
                    const MAX_INFLIGHT: usize = 8;
                    const WAY_OUTPUT_MULTIPLIER: usize = 10;
                    let way_budget = if config.way_inflight_budget > 0 {
                        config.way_inflight_budget
                    } else if locations_on_ways {
                        DEFAULT_WAY_BUDGET_LOCATIONS
                    } else {
                        DEFAULT_WAY_BUDGET
                    };
                    worker_handle = Some(std::thread::spawn(move || {
                        use rayon::prelude::*;
                        // Take refs outside loop - Copy into each move closure,
                        // avoids Arc::clone per spawn.
                        let nr_ref: Option<&NodeStoreReader> = nr_clone.as_deref();
                        let mr_ref = &*missing_ref_stats_clone;
                        let ds_ref = &*deferral_stats_clone;
                        // Byte-budgeted throttle: (count, estimated_bytes).
                        // Condvar wakes dispatcher when a task completes.
                        let inflight = std::sync::Mutex::new((0usize, 0usize));
                        let inflight_cvar = std::sync::Condvar::new();
                        let inflight_ref = &inflight;
                        let cvar_ref = &inflight_cvar;
                        rayon::in_place_scope(|s| {
                            while let Ok(block) = brx.recv() {
                                let raw_ways: Vec<RawWay> = block
                                    .elements()
                                    .filter_map(|e| match e {
                                        Element::Way(way) => {
                                            let tags: Vec<(String, String)> = way
                                                .tags()
                                                .map(|(k, v)| (k.to_string(), v.to_string()))
                                                .collect();
                                            if nr_ref.is_some() {
                                                // Standard PBF: collect node refs
                                                let node_refs: Vec<i64> = way.refs().collect();
                                                if node_refs.is_empty() {
                                                    return None;
                                                }
                                                Some(RawWay {
                                                    way_id: way.id(),
                                                    node_refs,
                                                    preserve_node_refs: Vec::new(),
                                                    coords_e7: Vec::new(),
                                                    tags,
                                                })
                                            } else {
                                                // Locations-on-ways: collect refs + coords directly
                                                let node_refs: Vec<i64> = way.refs().collect();
                                                let coords_e7: Vec<(i32, i32)> = way
                                                    .node_locations()
                                                    .map(|loc| {
                                                        (loc.decimicro_lat(), loc.decimicro_lon())
                                                    })
                                                    .collect();
                                                if coords_e7.is_empty() {
                                                    return None;
                                                }
                                                Some(RawWay {
                                                    way_id: way.id(),
                                                    node_refs,
                                                    preserve_node_refs: Vec::new(),
                                                    coords_e7,
                                                    tags,
                                                })
                                            }
                                        }
                                        _ => None,
                                    })
                                    .collect();
                                let mut raw_ways = raw_ways;
                                annotate_block_shared_node_refs(&mut raw_ways);
                                annotate_global_shared_node_refs(&mut raw_ways, &gsn);
                                let block_bytes = estimate_raw_ways_bytes(&raw_ways);
                                let block_cost = block_bytes * WAY_OUTPUT_MULTIPLIER;
                                // Wait for capacity: count limit and byte budget.
                                // Always allow at least one task - a single block that
                                // exceeds the byte budget must not deadlock the condvar
                                // (no in-flight tasks → no notify_one → permanent sleep).
                                {
                                    let mut guard = inflight_ref.lock().expect("inflight lock");
                                    guard = inflight_cvar
                                        .wait_while(guard, |&mut (count, bytes)| {
                                            count >= MAX_INFLIGHT
                                                || (count > 0 && bytes + block_cost > way_budget)
                                        })
                                        .expect("condvar wait");
                                    guard.0 += 1;
                                    guard.1 += block_cost;
                                    // Update HWM with current in-flight bytes (raw, not multiplied).
                                    way_hwm_clone.fetch_max(
                                        guard.1 / WAY_OUTPUT_MULTIPLIER,
                                        Ordering::Relaxed,
                                    );
                                }
                                let tx = rtx.clone();
                                #[allow(clippy::let_underscore_must_use)]
                                s.spawn(move |_| {
                                    let results: Vec<ProcessedWay> = raw_ways
                                        .into_par_iter()
                                        .map(|raw| {
                                            process_raw_way(
                                                &raw, nr_ref, mz, xz, &srl, ds_ref, mr_ref, &fcs,
                                                psf,
                                            )
                                        })
                                        .collect();
                                    let _ = tx.send(results);
                                    let mut guard = inflight_ref.lock().expect("inflight lock");
                                    guard.0 -= 1;
                                    guard.1 -= block_cost;
                                    cvar_ref.notify_one();
                                });
                            }
                        });
                        // Scope waits for all spawned tasks. rtx drops here →
                        // drain's rrx.recv() returns Err → drain exits.
                    }));

                    // Drain thread: owns way_index + sort_writer, writes results as they arrive.
                    // Runs concurrently with worker - main thread is free to forward blocks.
                    let mut wi = way_index.take().expect("way_index already taken");
                    let mut sw = sort_writer.take().expect("sort_writer already taken");
                    let ds_drain = std::sync::Arc::clone(&deferral_stats);
                    let srl_drain = config.seam_reconcile_layers;
                    drain_handle = Some(std::thread::spawn(move || {
                        let mut count: u64 = 0;
                        let mut fanout = FanoutStats::new();
                        while let Ok(results) = rrx.recv() {
                            count += drain_processed_ways(results, &mut wi, &mut sw, &mut fanout);
                            ds_drain.check_budgets(&srl_drain);
                        }
                        (wi, sw, count, fanout)
                    }));

                    block_tx = Some(btx);
                }

                // send() blocks if worker is still processing previous block (backpressure)
                block_tx
                    .as_ref()
                    .expect("worker not initialized")
                    .send(block)
                    .expect("worker thread panicked");
            }
            BlockType::Relations => {
                // Buffer relation blocks - defer processing until all PBF blocks
                // are consumed. Locations-on-ways PBFs can have way blocks after
                // relation blocks; processing relations inline would finalize the
                // way_index too early.
                relation_blocks.push(block);
            }
            BlockType::Empty | BlockType::Mixed => {}
        }
    }

    // No way blocks arrived (prepass result unused): join so a prepass error
    // still surfaces and the thread does not outlive the phase.
    if let Some(handle) = prepass_handle.take() {
        drop(
            handle
                .join()
                .map_err(|_| PipelineError("shared-node prepass thread panicked".to_string()))??,
        );
    }

    // Shut down worker + drain after all PBF blocks consumed.
    if block_tx.is_some() {
        drop(block_tx.take());
        if let Some(h) = worker_handle.take() {
            h.join().expect("worker thread panicked");
        }
        if let Some(h) = drain_handle.take() {
            let (wi, sw, count, way_fanout) = h.join().expect("drain thread panicked");
            way_index = Some(wi);
            sort_writer = Some(sw);
            features_emitted += count;
            fanout_stats.merge(&way_fanout);
        }
    }

    // Finalize way index after all way blocks are processed.
    if let Some(ref mut wi) = way_index {
        wi.finish_writing().expect("failed to finalize way index");
    }
    eprintln!("  Ways: {way_count}, Features so far: {features_emitted}");
    eprintln!("  Way index finalized, processing relations...");

    let relation_blocks_buffered = relation_blocks.len();
    // Process buffered relation blocks.
    for block in &relation_blocks {
        block.for_each_element(|element| {
            if let Element::Relation(rel) = element {
                rel_count += 1;
                if let Some(prepared) = prepare_relation(
                    &rel,
                    way_index
                        .as_ref()
                        .expect("way_index not returned from drain"),
                    &missing_ref_stats,
                ) {
                    rel_batch_bytes += estimate_prepared_rel_bytes(&prepared);
                    rel_batch.push(prepared);
                    if rel_batch_bytes > max_rel_batch_bytes {
                        max_rel_batch_bytes = rel_batch_bytes;
                    }
                    if rel_batch.len() >= REL_BATCH_SIZE || rel_batch_bytes >= rel_budget {
                        let batch =
                            std::mem::replace(&mut rel_batch, Vec::with_capacity(REL_BATCH_SIZE));
                        rel_batch_bytes = 0;
                        features_emitted += flush_rel_batch(
                            batch,
                            min_z,
                            max_z,
                            &config.seam_reconcile_layers,
                            &deferral_stats,
                            sort_writer
                                .as_mut()
                                .expect("sort_writer not returned from drain"),
                            &mut fanout_stats,
                            &config.fanout_caps,
                            config.polygon_simplify_factor,
                        );
                        deferral_stats.check_budgets(&config.seam_reconcile_layers);
                    }
                }
            }
        });
    }
    let rss_before_relation_drop = current_rss_kb();
    drop(relation_blocks);
    let rss_after_relation_drop = current_rss_kb();
    let relation_blocks_drop_rss_kb = rss_before_relation_drop
        .zip(rss_after_relation_drop)
        .map(|(before, after)| before.saturating_sub(after));

    if !rel_batch.is_empty() {
        features_emitted += flush_rel_batch(
            rel_batch,
            min_z,
            max_z,
            &config.seam_reconcile_layers,
            &deferral_stats,
            sort_writer
                .as_mut()
                .expect("sort_writer not returned from drain"),
            &mut fanout_stats,
            &config.fanout_caps,
            config.polygon_simplify_factor,
        );
        deferral_stats.check_budgets(&config.seam_reconcile_layers);
    }

    // Drop way_index to release compressed data + index memory before
    // ocean/sort/assemble phases.
    drop(way_index);

    eprintln!("  Nodes: {node_count}, Ways: {way_count}, Relations: {rel_count}");
    eprintln!("  Total features emitted: {features_emitted}");

    // Compute data extent in Mercator [0,1] with generous buffer for ocean overlap
    let data_bounds = if min_lat_e7 < max_lat_e7 {
        let crosses_dateline = crosses_antimeridian(
            min_lon_e7,
            max_lon_e7,
            min_lon_shifted_e7,
            max_lon_shifted_e7,
        );
        let west_lon_e7 = if crosses_dateline {
            -1_800_000_000
        } else {
            min_lon_e7
        };
        let east_lon_e7 = if crosses_dateline {
            1_800_000_000
        } else {
            max_lon_e7
        };
        let sw = geometry::project_e7(min_lat_e7, west_lon_e7);
        let ne = geometry::project_e7(max_lat_e7, east_lon_e7);
        // Add ~1 degree buffer (in Mercator space, roughly 1/360 ≈ 0.003)
        let buf = 0.01;
        MercBbox {
            min_x: if crosses_dateline {
                0.0
            } else {
                (sw.x - buf).max(0.0)
            },
            min_y: (ne.y - buf).max(0.0), // ne.y < sw.y in Mercator [0,1]
            max_x: if crosses_dateline {
                1.0
            } else {
                (ne.x + buf).min(1.0)
            },
            max_y: (sw.y + buf).min(1.0),
        }
    } else {
        // No nodes - full world
        MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        }
    };
    eprintln!(
        "  Data bounds (merc): x[{:.4}-{:.4}] y[{:.4}-{:.4}]",
        data_bounds.min_x, data_bounds.max_x, data_bounds.min_y, data_bounds.max_y
    );
    let max_way_inflight_bytes = way_hwm.load(Ordering::Relaxed);
    let missing_ref_snapshot = missing_ref_stats.snapshot();
    let sw = sort_writer.expect("sort_writer not returned from drain");
    let stats = Phase12Stats {
        node_count,
        way_count,
        rel_count,
        node_store_stats,
        max_way_inflight_bytes,
        max_rel_batch_bytes,
        relation_blocks_buffered,
        relation_blocks_drop_rss_kb,
        missing_refs: missing_ref_snapshot,
        deferral_stats,
        sort_records: sw.total_records(),
        sort_record_bytes: sw.total_record_bytes(),
        layer_records: *sw.layer_records(),
        layer_bytes: *sw.layer_bytes(),
        layer_zoom_records: Box::new(*sw.layer_zoom_records()),
        layer_zoom_bytes: Box::new(*sw.layer_zoom_bytes()),
        fanout_stats,
    };
    Ok((sw, data_bounds, stats))
}

// ---------------------------------------------------------------------------
// Node processing (point layers)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
pub(super) fn process_node(
    osm_id: u64,
    lat_e7: i32,
    lon_e7: i32,
    tags: &[(&str, &str)],
    min_zoom: u8,
    max_zoom: u8,
    records: &mut Vec<SortRecord>,
) -> u64 {
    let tag_helper = Tags(tags);
    let matches = shortbread::match_element(&tag_helper, OsmGeomType::Node);
    if matches.is_empty() {
        return 0;
    }

    let p = geometry::project_e7(lat_e7, lon_e7);
    let pbbox = MercBbox {
        min_x: p.x,
        min_y: p.y,
        max_x: p.x,
        max_y: p.y,
    };
    let mut count: u64 = 0;
    let mut geom_buf: Vec<u32> = Vec::new();
    let mut attrs_buf: Vec<u8> = Vec::new();

    for m in &matches {
        let z_lo = m.min_zoom.max(min_zoom);
        let z_hi = m.max_zoom.min(max_zoom);
        if z_lo > z_hi {
            continue;
        }

        for z in z_lo..=z_hi {
            encode_attrs_bytes(&mut attrs_buf, &m.attrs, z);
            geometry::for_each_tile_in_bbox(&pbbox, z, |tx, ty| {
                let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                let (px, py) = geometry::merc_to_tile_px(&p, tx, ty, z);
                mvt::encode_point(&mut geom_buf, px, py);
                push_sort_record(
                    tile_id,
                    osm_id,
                    m.layer,
                    GeomType::Point,
                    &geom_buf,
                    &attrs_buf,
                    records,
                );
                count += 1;
            });
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Way processing (line + polygon layers) - parallel batch processing
//
// Raw way data (node ref IDs + owned tags) is collected on the main thread,
// then dispatched to rayon where workers do the expensive work in parallel:
// node coord resolution (mmap reads - page faults spread across threads),
// tag matching, projection, simplification, clipping, MVT encoding.
// The serial post-rayon phase does only fast sequential I/O:
// way_index.put() + sort_writer.push().
// ---------------------------------------------------------------------------

/// Raw way data copied from PBF on the main thread. Tags are owned because
/// PBF element borrows don't survive the callback (same pattern as PreparedRelation).
// Tags use String not compact-string: short-lived, mimalloc handles small allocs efficiently.
pub(super) struct RawWay {
    pub(super) way_id: i64,
    pub(super) node_refs: Vec<i64>,
    pub(super) preserve_node_refs: Vec<i64>,
    pub(super) coords_e7: Vec<(i32, i32)>,
    pub(super) tags: Vec<(String, String)>,
}
const _: () = assert!(std::mem::size_of::<RawWay>() == 104);

/// Estimate heap bytes for a block of raw ways (struct + node_refs + tag strings).
pub(super) fn estimate_raw_ways_bytes(ways: &[RawWay]) -> usize {
    ways.iter()
        .map(|w| {
            std::mem::size_of::<RawWay>()
                + w.node_refs.len() * 8
                + w.preserve_node_refs.len() * 8
                + w.coords_e7.len() * 8
                + w.tags.len() * 48
                + w.tags.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
        })
        .sum()
}

/// Mark interior node refs that are shared by at least 2 ways in the same block.
///
/// This preserves common junction vertices during DP simplification without global
/// topology indexing. Block-local detection catches most local road intersections
/// because OSM PBF primitive blocks are spatially clustered.
/// Limitation: cross-block shared nodes are intentionally not detected here.
pub(super) fn annotate_block_shared_node_refs(raw_ways: &mut [RawWay]) {
    let mut counts: FxHashMap<i64, u8> = FxHashMap::default();
    for w in raw_ways.iter() {
        if w.node_refs.len() <= 2 {
            continue;
        }
        let is_closed = w.node_refs.len() >= 4 && w.node_refs.first() == w.node_refs.last();
        if is_closed {
            // Closed ring: shared-edge vertices can appear anywhere in the ring
            // except the duplicated closing vertex.
            for &node_id in &w.node_refs[..w.node_refs.len() - 1] {
                counts
                    .entry(node_id)
                    .and_modify(|c| *c = c.saturating_add(1))
                    .or_insert(1);
            }
        } else {
            // Open line: count ALL nodes including endpoints.
            // Endpoints are where ways connect - if two ways share an endpoint,
            // it must be pinned so DP simplification doesn't move it to different
            // positions in each way (which creates visible gaps at junctions).
            for &node_id in &w.node_refs {
                counts
                    .entry(node_id)
                    .and_modify(|c| *c = c.saturating_add(1))
                    .or_insert(1);
            }
        }
    }

    for w in raw_ways.iter_mut() {
        w.preserve_node_refs.clear();
        if w.node_refs.len() <= 2 {
            continue;
        }
        let is_closed = w.node_refs.len() >= 4 && w.node_refs.first() == w.node_refs.last();
        let scan_slice = if is_closed {
            &w.node_refs[..w.node_refs.len() - 1]
        } else {
            // Scan all nodes including endpoints - shared endpoints must be
            // pinned to prevent DP from creating gaps at way junctions.
            &w.node_refs[..]
        };
        for &node_id in scan_slice {
            if counts.get(&node_id).is_some_and(|&c| c >= 2)
                && !w.preserve_node_refs.contains(&node_id)
            {
                w.preserve_node_refs.push(node_id);
            }
        }
    }
}

/// First pass over the PBF: count node ref occurrences across all ways.
/// Returns the set of node IDs that appear in 2+ ways (junction nodes).
///
/// Uses `BlobFilter::only_ways()` to skip node/relation blobs entirely
/// (indexed PBFs skip decompression; non-indexed still parse cheaply).
/// No tag matching or coordinate resolution - just node ref counting.
#[hotpath::measure]
fn prepass_shared_nodes(
    pbf_path: &std::path::Path,
    decode_threads: usize,
) -> Result<FxHashSet<i64>, PipelineError> {
    let start = std::time::Instant::now();
    let reader = ElementReader::from_path(pbf_path)
        .map_err(|e| PipelineError(format!("prepass: failed to open PBF: {e}")))?
        .with_blob_filter(BlobFilter::only_ways())
        .decode_threads(decode_threads);

    let mut seen: FxHashSet<i64> = FxHashSet::default();
    let mut shared: FxHashSet<i64> = FxHashSet::default();

    for block_result in reader.into_blocks_pipelined() {
        let block =
            block_result.map_err(|e| PipelineError(format!("prepass: PBF read failed: {e}")))?;
        block.for_each_element(|element| {
            if let Element::Way(way) = element {
                for node_id in way.refs() {
                    if !seen.insert(node_id) {
                        shared.insert(node_id);
                    }
                }
            }
        });
    }

    let elapsed = start.elapsed();
    let seen_count = seen.len();
    drop(seen);
    eprintln!(
        "  Shared-node prepass: {:.1}s ({} unique nodes, {} shared)",
        elapsed.as_secs_f64(),
        seen_count,
        shared.len(),
    );
    Ok(shared)
}

/// Annotate ways with globally-shared node refs (cross-block junctions).
///
/// Supplements `annotate_block_shared_node_refs` which only detects junctions
/// within a single PBF block. Nodes in `global_shared` that appear in a way's
/// node refs are added to `preserve_node_refs` so DP simplification pins them.
fn annotate_global_shared_node_refs(raw_ways: &mut [RawWay], global_shared: &FxHashSet<i64>) {
    for w in raw_ways.iter_mut() {
        if w.node_refs.len() <= 2 {
            continue;
        }
        let is_closed = w.node_refs.len() >= 4 && w.node_refs.first() == w.node_refs.last();
        let scan_slice = if is_closed {
            &w.node_refs[..w.node_refs.len() - 1]
        } else {
            &w.node_refs[..]
        };
        for &node_id in scan_slice {
            if global_shared.contains(&node_id) && !w.preserve_node_refs.contains(&node_id) {
                w.preserve_node_refs.push(node_id);
            }
        }
    }
}

#[inline]
pub(super) fn lon_e7_shifted_360(lon_e7: i32) -> i64 {
    let lon = i64::from(lon_e7);
    if lon < 0 {
        lon + LON_E7_FULL_CIRCLE
    } else {
        lon
    }
}

#[inline]
pub(super) fn crosses_antimeridian(
    min_lon_e7: i32,
    max_lon_e7: i32,
    min_lon_shifted_e7: i64,
    max_lon_shifted_e7: i64,
) -> bool {
    let raw_span_e7 = i64::from(max_lon_e7) - i64::from(min_lon_e7);
    let shifted_span_e7 = max_lon_shifted_e7 - min_lon_shifted_e7;
    shifted_span_e7 < raw_span_e7
}

/// Result of parallel way processing: resolved coords (needed for way_index),
/// sort records (geometry output). Land mask is marked on rayon threads directly.
pub(super) struct ProcessedWay {
    pub(super) way_id: i64,
    pub(super) coords_e7: Vec<(i32, i32)>,
    pub(super) records: Vec<SortRecord>,
    /// Cap events from polygon emit: (layer_zoom_idx, bbox_tiles).
    pub(super) cap_events: Vec<(u16, u64, u64)>,
}

pub(super) struct WayWorkerScratch {
    pub(super) merc: Vec<Point>,
    pub(super) point_emit: PointEmitScratch,
    pub(super) line_emit: LineEmitScratch,
    pub(super) polygon_emit: PolygonEmitScratch,
}

impl WayWorkerScratch {
    pub(super) fn new() -> Self {
        Self {
            merc: Vec::new(),
            point_emit: PointEmitScratch::new(),
            line_emit: LineEmitScratch::new(),
            polygon_emit: PolygonEmitScratch::new(),
        }
    }
}

thread_local! {
    pub(super) static WAY_WORKER_SCRATCH: std::cell::RefCell<WayWorkerScratch> = std::cell::RefCell::new(WayWorkerScratch::new());
}

/// Drain a single batch of processed way results: write way_index entries,
/// push sort records. Returns feature count.
#[hotpath::measure]
pub(super) fn drain_processed_ways(
    results: Vec<ProcessedWay>,
    way_index: &mut WayIndex,
    sort_writer: &mut SortWriter,
    fanout: &mut FanoutStats,
) -> u64 {
    let mut count: u64 = 0;
    for pw in results {
        if !pw.coords_e7.is_empty() {
            way_index.put(pw.way_id, &pw.coords_e7);
        }
        record_fanout_from_records(&pw.records, fanout);
        // Harvest cap events from polygon emit.
        for &(idx, tiles, oid) in &pw.cap_events {
            let layer = idx as usize / 15;
            let zoom = idx as usize % 15;
            fanout.record_cap(layer, zoom, tiles, oid);
        }
        count += pw.records.len() as u64;
        // Panic: disk I/O failure is unrecoverable mid-pipeline.
        for record in pw.records {
            sort_writer.push(record).expect("sort push failed");
        }
    }
    count
}

/// Process a raw way on a rayon worker thread: resolve node coordinates,
/// match tags, and run geometry processing (projection, simplification,
/// clipping, MVT encoding).
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn process_raw_way(
    raw: &RawWay,
    node_reader: Option<&NodeStoreReader>,
    min_zoom: u8,
    max_zoom: u8,
    seam_reconcile_layers: &[u8],
    deferral_stats: &DeferralStats,
    missing_ref_stats: &MissingRefStatsAtomic,
    fanout_caps: &[u32],
    polygon_simplify_factor: f64,
) -> ProcessedWay {
    // Resolve node coordinates: either pre-resolved from locations-on-ways PBF,
    // or looked up via node store (the expensive mmap reads - now parallel).
    let (coords_e7, resolved_node_refs): (Vec<(i32, i32)>, Vec<i64>) = if !raw.coords_e7.is_empty()
    {
        (raw.coords_e7.clone(), raw.node_refs.clone())
    } else if let Some(nr) = node_reader {
        let mut missing_refs: usize = 0;
        let mut resolved: Vec<(i32, i32)> = Vec::with_capacity(raw.node_refs.len());
        let mut resolved_refs: Vec<i64> = Vec::with_capacity(raw.node_refs.len());
        for &id in &raw.node_refs {
            if let Some(coord) = nr.get(id) {
                resolved.push(coord);
                resolved_refs.push(id);
            } else {
                missing_refs += 1;
            }
        }
        if missing_refs > 0 {
            missing_ref_stats.record_way_missing_nodes(missing_refs);
        }
        (resolved, resolved_refs)
    } else {
        (Vec::new(), Vec::new())
    };

    if coords_e7.is_empty() || raw.tags.is_empty() {
        return ProcessedWay {
            way_id: raw.way_id,
            coords_e7,
            records: Vec::new(),
            cap_events: Vec::new(),
        };
    }

    // Tag matching - convert owned tags to borrowed refs (same pattern as
    // process_prepared_relation, pipeline.rs PreparedRelation handling)
    let is_closed = coords_e7.len() >= 4 && coords_e7.first() == coords_e7.last();
    let geom_type = if is_closed {
        OsmGeomType::ClosedWay
    } else {
        OsmGeomType::OpenWay
    };
    let tags_ref: Vec<(&str, &str)> = raw
        .tags
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let tag_helper = Tags(&tags_ref);
    let mut matches = shortbread::match_element(&tag_helper, geom_type);

    if matches.is_empty() {
        return ProcessedWay {
            way_id: raw.way_id,
            coords_e7,
            records: Vec::new(),
            cap_events: Vec::new(),
        };
    }

    #[allow(clippy::cast_sign_loss)]
    let osm_id = raw.way_id as u64;
    let mut records = Vec::new();
    let mut cap_events: Vec<(u16, u64, u64)> = Vec::new();
    let mut preserve_vertex_mask: Vec<bool> = vec![false; coords_e7.len()];
    if !raw.preserve_node_refs.is_empty() {
        let preserve_nodes: FxHashSet<i64> = raw.preserve_node_refs.iter().copied().collect();
        for (i, node_id) in resolved_node_refs.iter().enumerate() {
            if preserve_nodes.contains(node_id) {
                preserve_vertex_mask[i] = true;
            }
        }
    }

    WAY_WORKER_SCRATCH.with(|cell| {
        let scratch = &mut *cell.borrow_mut();
        scratch.merc.clear();
        scratch.merc.extend(
            coords_e7
                .iter()
                .map(|&(lat, lon)| geometry::project_e7(lat, lon)),
        );
        let _ = unwrap_antimeridian_path(&mut scratch.merc, is_closed);

        let merc = scratch.merc.as_slice();
        let merc_bbox_val = merc_bbox(merc);

        // Enrich polygon matches with area-dependent data (way_area, min_zoom overrides)
        if is_closed {
            let area_m2 = geometry::area_sq_meters(merc);
            enrich_polygon_matches(&mut matches, area_m2);
        }

        for m in &matches {
            let z_lo = m.min_zoom.max(min_zoom);
            let z_hi = m.max_zoom.min(max_zoom);
            if z_lo > z_hi {
                continue;
            }

            match m.geom_expect {
                GeomExpect::Point
                | GeomExpect::PolygonCentroid
                | GeomExpect::PolygonPointOnSurface => {
                    emit_point_or_centroid(
                        osm_id,
                        merc,
                        None,
                        &merc_bbox_val,
                        m,
                        z_lo,
                        z_hi,
                        &mut records,
                        &mut scratch.point_emit,
                    );
                }
                GeomExpect::Line => {
                    for shift in antimeridian_shifts_for_bbox(&merc_bbox_val) {
                        if shift == 0.0 {
                            emit_line_feature(
                                osm_id,
                                merc,
                                &preserve_vertex_mask,
                                m,
                                z_lo,
                                z_hi,
                                &mut records,
                                &mut scratch.line_emit,
                            );
                        } else {
                            let shifted: Vec<Point> = merc
                                .iter()
                                .map(|p| Point {
                                    x: p.x + shift,
                                    y: p.y,
                                })
                                .collect();
                            emit_line_feature(
                                osm_id,
                                &shifted,
                                &preserve_vertex_mask,
                                m,
                                z_lo,
                                z_hi,
                                &mut records,
                                &mut scratch.line_emit,
                            );
                        }
                    }
                }
                GeomExpect::Polygon => {
                    let sr = seam_reconcile_layers[m.layer as usize];
                    let fc = fanout_caps.get(m.layer as usize).copied().unwrap_or(0);
                    for shift in antimeridian_shifts_for_bbox(&merc_bbox_val) {
                        if shift == 0.0 {
                            emit_polygon_feature(
                                osm_id,
                                merc,
                                &preserve_vertex_mask,
                                m,
                                z_lo,
                                z_hi,
                                &mut records,
                                &mut scratch.polygon_emit,
                                sr,
                                Some(deferral_stats),
                                fc,
                                polygon_simplify_factor,
                            );
                        } else {
                            let shifted: Vec<Point> = merc
                                .iter()
                                .map(|p| Point {
                                    x: p.x + shift,
                                    y: p.y,
                                })
                                .collect();
                            emit_polygon_feature(
                                osm_id,
                                &shifted,
                                &preserve_vertex_mask,
                                m,
                                z_lo,
                                z_hi,
                                &mut records,
                                &mut scratch.polygon_emit,
                                sr,
                                Some(deferral_stats),
                                fc,
                                polygon_simplify_factor,
                            );
                        }
                    }
                }
            }
        }
        // Collect cap events from polygon scratch before leaving the borrow.
        cap_events = std::mem::take(&mut scratch.polygon_emit.cap_events);
    });

    ProcessedWay {
        way_id: raw.way_id,
        coords_e7,
        records,
        cap_events,
    }
}
