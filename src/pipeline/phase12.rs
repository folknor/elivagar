use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::debug::{BUSY, WAIT, wait_span};
use crate::geometry::{self, MercBbox, Point, merc_bbox};
use crate::mvt::{self, GeomType};
use crate::node_index::{NodeIndex, NodeStore, NodeStoreReader, SortedNodeStore};
use crate::pmtiles_writer;
use crate::shortbread::{self, GeomExpect, OsmGeomType, Tags};
use crate::sort::{SortRecord, SortWriter};
use crate::way_index::WayIndex;
use crate::wire_format::encode_attrs_bytes;
use pbfhogg::{BlobFilter, BlockType, Element, ElementReader, MemberId, PrimitiveBlock, Way};

use super::emit::{
    LineEmitScratch, PointEmitScratch, PolygonEmitScratch, RecordSink,
    antimeridian_shifts_for_bbox, emit_line_feature, emit_point_or_centroid, emit_polygon_feature,
    enrich_polygon_matches, push_sort_record, unwrap_antimeridian_path,
};
use super::relations::process_relation_blocks;
use super::stats::{
    DeferralStats, FanoutStats, MissingRefStatsAtomic, Phase12Stats,
    record_fanout_from_payload_records,
};
use super::{PipelineError, SORT_CHUNKS_DIR, TilegenConfig, current_rss_kb};

pub(super) const LON_E7_FULL_CIRCLE: i64 = 3_600_000_000;
/// Default memory budget per sort chunk (1 GB).
pub(super) const DEFAULT_SORT_CHUNK_SIZE: usize = 1 << 30;
/// Default way in-flight budget for the standard node-store path.
pub(super) const DEFAULT_WAY_BUDGET: usize = 128 * 1024 * 1024; // 128 MB
/// Default way in-flight budget for locations-on-ways mode. The budget is
/// compared against estimated cost (raw bytes x WAY_OUTPUT_MULTIPLIER), so
/// 768M means ~77MB of raw block+plan bytes in flight. Measured on germany
/// locations at 256M: way_budget wait 31.9s (45% of wall) with the feed
/// starved at ~25MB raw - the byte budget, not the count ceiling, was the
/// binding constraint once plan building moved into the tasks.
pub(super) const DEFAULT_WAY_BUDGET_LOCATIONS: usize = 768 * 1024 * 1024; // 768 MB
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
    // RAM-ledger sizes of the planet-scaling structures phase12 holds:
    // buffered relation blocks (decompressed bytes), the relation plan's
    // member-way set, and the global shared-node pin set.
    let mut relation_blocks_bytes: usize = 0;
    let mut relation_plan_needed_ways: usize = 0;
    let mut global_shared_nodes: usize = 0;

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
    let prepass_tmp_dir = config.tmp_dir.join("shared_node_prepass");
    let prepass_sort_budget = sort_chunk_budget;
    let mut prepass_handle: Option<std::thread::JoinHandle<Result<FxHashSet<i64>, PipelineError>>> =
        if config.global_shared_node_pins {
            Some(std::thread::spawn(move || {
                prepass_shared_nodes(
                    &prepass_pbf_path,
                    decode_threads,
                    &prepass_tmp_dir,
                    prepass_sort_budget,
                )
            }))
        } else {
            eprintln!("  Global shared-node prepass disabled - using block-local pins");
            None
        };
    let relation_plan_pbf_path = config.pbf_path.clone();
    let mut relation_plan_handle: Option<
        std::thread::JoinHandle<Result<RelationPlan, PipelineError>>,
    > = Some(std::thread::spawn(move || {
        prepass_relation_plan(&relation_plan_pbf_path, decode_threads)
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
                // Node block - process inline. This runs on the ordered pbfhogg
                // consumer thread: its busy time is what decode workers stall
                // behind (pipeline_decoded_send), so it is timed per block.
                let _busy = wait_span(&BUSY.phase12_node_blocks);
                block.for_each_element(|element| match element {
                    Element::DenseNode(node) => handle_node!(node),
                    Element::Node(node) => handle_node!(node),
                    _ => {}
                });
            }
            BlockType::Ways => {
                // Way block - send entire block to worker thread.
                // Count ways from block (elements() re-parses from bytes, cheap).
                {
                    let _busy = wait_span(&BUSY.phase12_way_count);
                    way_count += block
                        .elements()
                        .filter(|e| matches!(e, Element::Way(_)))
                        .count() as u64;
                }

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
                    // Capacity must be at least the in-flight task ceiling: rayon tasks block on send()
                    // while holding a rayon thread. If capacity < inflight tasks,
                    // blocked senders tie up all rayon threads → worker (which runs
                    // inside rayon::in_place_scope) can't make progress → deadlock.
                    let max_inflight = config.threads.max(8);
                    let (rtx, rrx) = std::sync::mpsc::sync_channel::<WayTaskResult>(max_inflight);
                    let nr_clone = nr.clone();
                    let way_hwm_clone = std::sync::Arc::clone(&way_hwm);
                    let missing_ref_stats_clone = std::sync::Arc::clone(&missing_ref_stats);
                    let deferral_stats_clone = std::sync::Arc::clone(&deferral_stats);
                    let mz = min_z;
                    let xz = max_z;
                    let srl = config.seam_reconcile_layers;
                    let fcs = config.fanout_caps;
                    let psf = config.polygon_simplify_factor;
                    let way_chunk_dir = config.tmp_dir.join(SORT_CHUNKS_DIR);
                    let way_chunk_size = sort_chunk_budget;
                    let way_chunk_compression = config.compress_sort_chunks;
                    let way_chunk_id = std::sync::Arc::new(AtomicUsize::new(
                        sort_writer
                            .as_ref()
                            .expect("sort_writer taken before way worker")
                            .chunk_count(),
                    ));
                    // The drain writer flushes leftover tails into the same chunk
                    // directory concurrently with the way tasks. Share ONE chunk-number
                    // allocator between them so a drain flush and a task flush never
                    // claim the same chunk_NNNN.bin.
                    let drain_chunk_id = std::sync::Arc::clone(&way_chunk_id);
                    // First point where the shared-node set is needed: join
                    // the prepass thread spawned before the node phase. The
                    // joins block the ordered consumer, so they are stall time.
                    let prepass_join_guard = wait_span(&WAIT.prepass_join);
                    let gsn: std::sync::Arc<FxHashSet<i64>> =
                        if let Some(handle) = prepass_handle.take() {
                            std::sync::Arc::new(handle.join().map_err(|_| {
                                PipelineError("shared-node prepass thread panicked".to_string())
                            })??)
                        } else {
                            std::sync::Arc::new(FxHashSet::default())
                        };
                    let relation_plan = std::sync::Arc::new(
                        relation_plan_handle
                            .take()
                            .expect("relation prepass joined twice")
                            .join()
                            .map_err(|_| {
                                PipelineError("relation prepass thread panicked".to_string())
                            })??,
                    );
                    drop(prepass_join_guard);
                    global_shared_nodes = gsn.len();
                    relation_plan_needed_ways = relation_plan.needed_ways.len();
                    let relation_plan_clone = std::sync::Arc::clone(&relation_plan);
                    // Multi-block overlap: rayon::scope allows multiple blocks' ways
                    // in the pool simultaneously. Byte-budgeted in-flight control
                    // limits total estimated memory, with a count ceiling as safety net.
                    const WAY_OUTPUT_MULTIPLIER: usize = 10;
                    let way_budget = if config.way_inflight_budget > 0 {
                        config.way_inflight_budget
                    } else if locations_on_ways {
                        DEFAULT_WAY_BUDGET_LOCATIONS
                    } else {
                        DEFAULT_WAY_BUDGET
                    };
                    worker_handle = Some(std::thread::spawn(move || {
                        // Take refs outside loop - Copy into each move closure,
                        // avoids Arc::clone per spawn.
                        let nr_ref: Option<&NodeStoreReader> = nr_clone.as_deref();
                        let mr_ref = &*missing_ref_stats_clone;
                        let ds_ref = &*deferral_stats_clone;
                        let rp_ref = &*relation_plan_clone;
                        let chunk_dir_base = way_chunk_dir;
                        let chunk_size = way_chunk_size;
                        let chunk_compression = way_chunk_compression;
                        let chunk_id_base = way_chunk_id;
                        // Byte-budgeted throttle: (count, estimated_bytes).
                        // Condvar wakes dispatcher when a task completes.
                        let inflight = std::sync::Mutex::new((0usize, 0usize));
                        let inflight_cvar = std::sync::Condvar::new();
                        let inflight_ref = &inflight;
                        let cvar_ref = &inflight_cvar;
                        let gsn_ref = &*gsn;
                        // Pool of accumulators shared across block tasks. Accs
                        // live for the whole way phase (not one block), so the
                        // bulk of the record volume self-flushes to partitioned
                        // chunk files at WAY_ACC_FLUSH_BYTES instead of funneling
                        // through the drain thread's serial sort_writer pushes -
                        // measured on germany locations: 12.3 GB of records
                        // through one thread, 141s of tasks blocked on
                        // way_result_send behind it. The drain keeps way_index
                        // ownership; per-block results carry only the way_puts.
                        // Buffered ceiling: one acc per concurrently running
                        // task, each under WAY_ACC_FLUSH_BYTES.
                        const WAY_ACC_FLUSH_BYTES: usize = 64 * 1024 * 1024;
                        let acc_flush_bytes = chunk_size.min(WAY_ACC_FLUSH_BYTES);
                        let acc_pool: std::sync::Mutex<Vec<WayAcc>> =
                            std::sync::Mutex::new(Vec::new());
                        let acc_pool_ref = &acc_pool;
                        rayon::in_place_scope(|s| {
                            while let Ok(block) = brx.recv() {
                                // Plan build happens inside the spawned task, not
                                // here: this loop is the pipeline stage the ordered
                                // consumer blocks behind (way_block_send), so any
                                // serial work here rate-limits the whole PBF read.
                                // Measured on germany locations: 26.9s of serial
                                // build_way_plans - half of phase12.
                                //
                                // The budget reservation therefore uses the block's
                                // decompressed size alone; the task adds the plans'
                                // measured bytes once built (bounded overshoot: at
                                // most max_inflight blocks' plan bytes escape the
                                // wait below).
                                let block_cost =
                                    block.decompressed_size() * WAY_OUTPUT_MULTIPLIER;
                                // Wait for capacity: count limit and byte budget.
                                // Always allow at least one task - a single block that
                                // exceeds the byte budget must not deadlock the condvar
                                // (no in-flight tasks → no notify_one → permanent sleep).
                                {
                                    let _wait = wait_span(&WAIT.way_budget);
                                    let mut guard = inflight_ref.lock().expect("inflight lock");
                                    guard = inflight_cvar
                                        .wait_while(guard, |&mut (count, bytes)| {
                                            count >= max_inflight
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
                                let chunk_dir = chunk_dir_base.clone();
                                let chunk_id = std::sync::Arc::clone(&chunk_id_base);
                                let way_hwm_task = std::sync::Arc::clone(&way_hwm_clone);
                                #[allow(clippy::let_underscore_must_use)]
                                s.spawn(move |_| {
                                    // Now summed across rayon workers, not a serial
                                    // stage: read phase12_plan_build_ns as thread-time.
                                    let plan_busy = wait_span(&BUSY.phase12_plan_build);
                                    let plans = build_way_plans(&block, gsn_ref);
                                    drop(plan_busy);
                                    let plan_cost =
                                        estimate_way_plans_bytes(&plans) * WAY_OUTPUT_MULTIPLIER;
                                    {
                                        // Account the plans' real bytes without waiting:
                                        // blocking a rayon task on the budget condvar can
                                        // deadlock the pool (every thread waiting, no
                                        // completions to free budget).
                                        let mut guard =
                                            inflight_ref.lock().expect("inflight lock");
                                        guard.1 += plan_cost;
                                        way_hwm_task.fetch_max(
                                            guard.1 / WAY_OUTPUT_MULTIPLIER,
                                            Ordering::Relaxed,
                                        );
                                    }
                                    let mut acc = acc_pool_ref
                                        .lock()
                                        .expect("way acc pool lock")
                                        .pop()
                                        .unwrap_or_else(|| WayAcc::new(chunk_compression));
                                    let mut plans = plans.into_iter();
                                    for element in block.elements() {
                                        let Element::Way(way) = element else {
                                            continue;
                                        };
                                        let Some(plan) = plans.next() else {
                                            continue;
                                        };
                                        debug_assert_eq!(
                                            plan.way_id,
                                            way.id(),
                                            "way plan misaligned with block ways"
                                        );
                                        let is_member = rp_ref.needed_ways.contains(&plan.way_id);
                                        process_planned_way_into(
                                            &way, &plan, is_member, nr_ref, mz, xz, &srl, ds_ref,
                                            mr_ref, &fcs, psf, &mut acc,
                                        );
                                        if acc.bytes >= acc_flush_bytes {
                                            acc.flush(&chunk_dir, &chunk_id);
                                        }
                                    }
                                    // Only the way_index puts go to the drain per
                                    // block; records stay in the pooled acc.
                                    let way_puts = std::mem::take(&mut acc.way_puts);
                                    ds_ref.check_budgets(&srl);
                                    acc_pool_ref
                                        .lock()
                                        .expect("way acc pool lock")
                                        .push(acc);
                                    if !way_puts.is_empty() {
                                        // Blocked here means the drain thread is the
                                        // choke - tasks queue behind its result channel
                                        // while holding a rayon thread.
                                        let _wait = wait_span(&WAIT.way_result_send);
                                        let _ = tx.send(WayTaskResult {
                                            chunk_paths: Vec::new(),
                                            count: 0,
                                            sink: RecordSink::new(),
                                            fanout: FanoutStats::new(),
                                            way_puts,
                                        });
                                    }
                                    let mut guard = inflight_ref.lock().expect("inflight lock");
                                    guard.0 -= 1;
                                    guard.1 -= block_cost + plan_cost;
                                    cvar_ref.notify_one();
                                });
                            }
                        });
                        // Scope waited for all tasks; the pooled accs hold each
                        // worker's residual records (below the flush threshold).
                        // Ship them to the drain through the same result path so
                        // they merge into sort_writer's normal buffering instead
                        // of becoming tiny chunk files.
                        for acc in acc_pool.into_inner().expect("way acc pool lock") {
                            let result = acc.finish();
                            rtx.send(result).expect("drain thread hung up early");
                        }
                        // rtx drops here → drain's rrx.recv() returns Err →
                        // drain exits.
                    }));

                    // Drain thread: owns way_index + sort_writer, writes results as they arrive.
                    // Runs concurrently with worker - main thread is free to forward blocks.
                    let mut wi = way_index.take().expect("way_index already taken");
                    let mut sw = sort_writer.take().expect("sort_writer already taken");
                    let ds_drain = std::sync::Arc::clone(&deferral_stats);
                    let srl_drain = config.seam_reconcile_layers;
                    drain_handle = Some(std::thread::spawn(move || {
                        sw.attach_chunk_counter(drain_chunk_id);
                        let mut count: u64 = 0;
                        let mut fanout = FanoutStats::new();
                        while let Ok(results) = rrx.recv() {
                            let _busy = wait_span(&BUSY.phase12_drain);
                            count += drain_way_task_result(results, &mut wi, &mut sw, &mut fanout);
                            ds_drain.check_budgets(&srl_drain);
                        }
                        // All tasks have finished allocating (rtx dropped closed the
                        // channel); resync chunk_count so ocean/relations/from_dir agree.
                        sw.detach_chunk_counter();
                        (wi, sw, count, fanout)
                    }));

                    block_tx = Some(btx);
                }

                // send() blocks if worker is still processing previous block (backpressure)
                let _wait = wait_span(&WAIT.way_block_send);
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
                relation_blocks_bytes += block.decompressed_size();
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
    if let Some(handle) = relation_plan_handle.take() {
        drop(
            handle
                .join()
                .map_err(|_| PipelineError("relation prepass thread panicked".to_string()))??,
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
    // Process buffered relation blocks. Tail of phase12: one streamed
    // parallel pass over all relations (prepare interleaved with emit, a
    // single end barrier). The span deliberately includes the rayon fan-out -
    // it is wall time appended to the phase either way.
    let relation_tail_busy = wait_span(&BUSY.phase12_relation_tail);
    let relation_tail = process_relation_blocks(
        &relation_blocks,
        way_index
            .as_ref()
            .expect("way_index not returned from drain"),
        &missing_ref_stats,
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
    rel_count += relation_tail.rel_count;
    features_emitted += relation_tail.features;
    let max_rel_inflight_bytes = relation_tail.max_inflight_bytes;
    deferral_stats.check_budgets(&config.seam_reconcile_layers);
    drop(relation_tail_busy);

    let rss_before_relation_drop = current_rss_kb();
    drop(relation_blocks);
    let rss_after_relation_drop = current_rss_kb();
    let relation_blocks_drop_rss_kb = rss_before_relation_drop
        .zip(rss_after_relation_drop)
        .map(|(before, after)| before.saturating_sub(after));

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
        max_rel_inflight_bytes,
        relation_blocks_buffered,
        relation_blocks_bytes,
        relation_plan_needed_ways,
        global_shared_nodes,
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

/// Legacy raw way fixture used by phase12 unit tests.
#[cfg(test)]
#[allow(dead_code)]
pub(super) struct RawWay {
    pub(super) way_id: i64,
    pub(super) node_refs: Vec<i64>,
    pub(super) preserve_node_refs: Vec<i64>,
    pub(super) coords_e7: Vec<(i32, i32)>,
    pub(super) tags: Vec<(String, String)>,
}
#[cfg(test)]
const _: () = assert!(std::mem::size_of::<RawWay>() == 104);

/// Estimate heap bytes for a block of raw ways (struct + node_refs + tag strings).
#[cfg(test)]
#[allow(dead_code)]
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

/// The node refs of a way that participate in shared-junction detection.
///
/// Ways with <= 2 refs never contribute or receive a pin. For a closed ring the
/// duplicated closing vertex is dropped (it is the same node as the first). For
/// an open line ALL nodes count, endpoints included - shared endpoints are where
/// ways connect and must be pinned so DP simplification does not move them to
/// different positions in each way (which creates visible gaps at junctions).
fn shared_scan_slice(node_refs: &[i64]) -> &[i64] {
    if node_refs.len() <= 2 {
        return &[];
    }
    let is_closed = node_refs.len() >= 4 && node_refs.first() == node_refs.last();
    if is_closed {
        &node_refs[..node_refs.len() - 1]
    } else {
        node_refs
    }
}

/// Count block-local node-ref occurrences across every way's scan slice.
fn shared_node_counts<'a>(ways: impl Iterator<Item = &'a [i64]>) -> FxHashMap<i64, u8> {
    let mut counts: FxHashMap<i64, u8> = FxHashMap::default();
    for refs in ways {
        for &node_id in shared_scan_slice(refs) {
            counts
                .entry(node_id)
                .and_modify(|c| *c = c.saturating_add(1))
                .or_insert(1);
        }
    }
    counts
}

/// Compute the preserve set for one way: refs that are shared by 2+ ways in this
/// block, or (when `global_shared` is supplied) known cross-block junctions.
fn preserve_refs_for_way(
    node_refs: &[i64],
    counts: &FxHashMap<i64, u8>,
    global_shared: Option<&FxHashSet<i64>>,
) -> Vec<i64> {
    let mut preserve: Vec<i64> = Vec::new();
    for &node_id in shared_scan_slice(node_refs) {
        let shared = counts.get(&node_id).is_some_and(|&c| c >= 2)
            || global_shared.is_some_and(|g| g.contains(&node_id));
        if shared && !preserve.contains(&node_id) {
            preserve.push(node_id);
        }
    }
    preserve
}

/// Mark interior node refs that are shared by at least 2 ways in the same block.
///
/// This preserves common junction vertices during DP simplification without global
/// topology indexing. Block-local detection catches most local road intersections
/// because OSM PBF primitive blocks are spatially clustered.
/// Limitation: cross-block shared nodes are intentionally not detected here.
///
/// Delegates to the same `shared_node_counts` / `preserve_refs_for_way` helpers
/// as the production `build_way_plans`, so this test wrapper cannot drift from
/// the code path the pipeline actually runs.
#[cfg(test)]
pub(super) fn annotate_block_shared_node_refs(raw_ways: &mut [RawWay]) {
    let counts = shared_node_counts(raw_ways.iter().map(|w| w.node_refs.as_slice()));
    for w in raw_ways.iter_mut() {
        w.preserve_node_refs = preserve_refs_for_way(&w.node_refs, &counts, None);
    }
}

pub(super) struct WayPlan {
    pub(super) way_id: i64,
    pub(super) node_refs: Vec<i64>,
    pub(super) preserve_node_refs: Vec<i64>,
}

fn estimate_way_plans_bytes(plans: &[WayPlan]) -> usize {
    plans
        .iter()
        .map(|p| {
            std::mem::size_of::<WayPlan>() + p.node_refs.len() * 8 + p.preserve_node_refs.len() * 8
        })
        .sum()
}

fn build_way_plans(block: &PrimitiveBlock, global_shared: &FxHashSet<i64>) -> Vec<WayPlan> {
    let mut plans: Vec<WayPlan> = block
        .elements()
        .filter_map(|element| {
            let Element::Way(way) = element else {
                return None;
            };
            // Emit a plan for EVERY way element (even one with zero node refs).
            // The task loop below zips this list positionally against
            // `block.ways()`; dropping any way here would shift every following
            // plan onto the wrong way. Empty-ref ways resolve to empty coords and
            // are dropped inside `process_planned_way_into`.
            Some(WayPlan {
                way_id: way.id(),
                node_refs: way.refs().collect(),
                preserve_node_refs: Vec::new(),
            })
        })
        .collect();

    let counts = shared_node_counts(plans.iter().map(|p| p.node_refs.as_slice()));
    for plan in &mut plans {
        plan.preserve_node_refs =
            preserve_refs_for_way(&plan.node_refs, &counts, Some(global_shared));
    }
    plans
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
    tmp_dir: &std::path::Path,
    chunk_budget: usize,
) -> Result<FxHashSet<i64>, PipelineError> {
    let start = std::time::Instant::now();
    match std::fs::remove_dir_all(tmp_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(PipelineError(format!(
                "prepass: remove old scratch dir failed: {e}"
            )));
        }
    }
    std::fs::create_dir_all(tmp_dir)
        .map_err(|e| PipelineError(format!("prepass: create scratch dir failed: {e}")))?;
    let reader = ElementReader::from_path(pbf_path)
        .map_err(|e| PipelineError(format!("prepass: failed to open PBF: {e}")))?
        .with_blob_filter(BlobFilter::only_ways())
        .decode_threads(decode_threads);

    let mut refs: Vec<u64> = Vec::with_capacity((chunk_budget / 8).clamp(1024, 8_000_000));
    let mut chunk_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut total_refs: u64 = 0;
    let flush_refs = |refs: &mut Vec<u64>,
                      chunk_paths: &mut Vec<std::path::PathBuf>|
     -> Result<(), PipelineError> {
        if refs.is_empty() {
            return Ok(());
        }
        refs.sort_unstable();
        let path = tmp_dir.join(format!("refs_{:04}.bin", chunk_paths.len()));
        let file = std::fs::File::create(&path)
            .map_err(|e| PipelineError(format!("prepass: create ref chunk failed: {e}")))?;
        let mut writer = std::io::BufWriter::with_capacity(1 << 20, file);
        use std::io::Write;
        for key in refs.iter() {
            writer
                .write_all(&key.to_le_bytes())
                .map_err(|e| PipelineError(format!("prepass: write ref chunk failed: {e}")))?;
        }
        writer
            .flush()
            .map_err(|e| PipelineError(format!("prepass: flush ref chunk failed: {e}")))?;
        refs.clear();
        chunk_paths.push(path);
        Ok(())
    };

    let target_refs = (chunk_budget / 8).max(1024);
    for block_result in reader.into_blocks_pipelined() {
        let block =
            block_result.map_err(|e| PipelineError(format!("prepass: PBF read failed: {e}")))?;
        for element in block.elements() {
            if let Element::Way(way) = element {
                for node_id in way.refs() {
                    refs.push(encode_signed_i64_key(node_id));
                    total_refs += 1;
                    if refs.len() >= target_refs {
                        flush_refs(&mut refs, &mut chunk_paths)?;
                    }
                }
            }
        }
    }
    flush_refs(&mut refs, &mut chunk_paths)?;

    let mut shared: FxHashSet<i64> = FxHashSet::default();
    let mut readers = Vec::with_capacity(chunk_paths.len());
    let mut heap = std::collections::BinaryHeap::new();
    for path in &chunk_paths {
        let mut reader = NodeRefChunkReader::open(path)?;
        if let Some(key) = reader.next_key()? {
            heap.push(NodeRefHeapEntry {
                key,
                chunk_idx: readers.len(),
            });
        }
        readers.push(reader);
    }

    let mut unique_count: u64 = 0;
    let mut prev: Option<u64> = None;
    let mut prev_count: u8 = 0;
    while let Some(entry) = heap.pop() {
        if prev == Some(entry.key) {
            prev_count = prev_count.saturating_add(1);
        } else {
            if let Some(key) = prev {
                unique_count += 1;
                if prev_count >= 2 {
                    shared.insert(decode_signed_i64_key(key));
                }
            }
            prev = Some(entry.key);
            prev_count = 1;
        }
        if let Some(next) = readers[entry.chunk_idx].next_key()? {
            heap.push(NodeRefHeapEntry {
                key: next,
                chunk_idx: entry.chunk_idx,
            });
        }
    }
    if let Some(key) = prev {
        unique_count += 1;
        if prev_count >= 2 {
            shared.insert(decode_signed_i64_key(key));
        }
    }
    shared.shrink_to_fit();
    std::fs::remove_dir_all(tmp_dir)
        .map_err(|e| PipelineError(format!("prepass: remove scratch dir failed: {e}")))?;

    let elapsed = start.elapsed();
    eprintln!(
        "  Shared-node prepass: {:.1}s ({} refs, {} unique nodes, {} shared)",
        elapsed.as_secs_f64(),
        total_refs,
        unique_count,
        shared.len(),
    );
    Ok(shared)
}

#[inline]
#[allow(clippy::cast_sign_loss)]
fn encode_signed_i64_key(value: i64) -> u64 {
    (value as u64) ^ (1_u64 << 63)
}

#[inline]
#[allow(clippy::cast_possible_wrap)]
fn decode_signed_i64_key(key: u64) -> i64 {
    (key ^ (1_u64 << 63)) as i64
}

struct NodeRefChunkReader {
    reader: std::io::BufReader<std::fs::File>,
    buf: [u8; 8],
}

impl NodeRefChunkReader {
    fn open(path: &std::path::Path) -> Result<Self, PipelineError> {
        let file = std::fs::File::open(path)
            .map_err(|e| PipelineError(format!("prepass: open ref chunk failed: {e}")))?;
        Ok(Self {
            reader: std::io::BufReader::with_capacity(256 * 1024, file),
            buf: [0; 8],
        })
    }

    fn next_key(&mut self) -> Result<Option<u64>, PipelineError> {
        use std::io::Read;
        match self.reader.read_exact(&mut self.buf) {
            Ok(()) => Ok(Some(u64::from_le_bytes(self.buf))),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(PipelineError(format!(
                "prepass: read ref chunk failed: {e}"
            ))),
        }
    }
}

#[derive(Eq, PartialEq)]
struct NodeRefHeapEntry {
    key: u64,
    chunk_idx: usize,
}

impl Ord for NodeRefHeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.chunk_idx.cmp(&self.chunk_idx))
    }
}

impl PartialOrd for NodeRefHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub(super) struct RelationPlan {
    pub(super) needed_ways: FxHashSet<i64>,
}

#[hotpath::measure]
fn prepass_relation_plan(
    pbf_path: &std::path::Path,
    decode_threads: usize,
) -> Result<RelationPlan, PipelineError> {
    let start = std::time::Instant::now();
    let reader = ElementReader::from_path(pbf_path)
        .map_err(|e| PipelineError(format!("relation prepass: failed to open PBF: {e}")))?
        .with_blob_filter(BlobFilter::only_relations())
        .decode_threads(decode_threads);

    let mut needed_ways: FxHashSet<i64> = FxHashSet::default();
    let mut matched_relations: u64 = 0;
    for block_result in reader.into_blocks_pipelined() {
        let block = block_result
            .map_err(|e| PipelineError(format!("relation prepass: PBF read failed: {e}")))?;
        block.for_each_element(|element| {
            let Element::Relation(rel) = element else {
                return;
            };
            let mut rel_type = "";
            for (k, v) in rel.tags() {
                if k == "type" {
                    rel_type = v;
                    break;
                }
            }
            if rel_type != "multipolygon" && rel_type != "boundary" {
                return;
            }
            let tags: smallvec::SmallVec<[(&str, &str); 16]> = rel.tags().collect();
            let tag_helper = Tags(&tags);
            if shortbread::match_element(&tag_helper, OsmGeomType::MultiPolygon).is_empty() {
                return;
            }
            matched_relations += 1;
            for member in rel.members() {
                if let MemberId::Way(way_id) = member.id {
                    needed_ways.insert(way_id);
                }
            }
        });
    }
    needed_ways.shrink_to_fit();

    eprintln!(
        "  Relation prepass: {:.1}s ({} matching relations, {} member ways)",
        start.elapsed().as_secs_f64(),
        matched_relations,
        needed_ways.len(),
    );
    Ok(RelationPlan { needed_ways })
}

/// Annotate ways with globally-shared node refs (cross-block junctions).
///
/// Supplements `annotate_block_shared_node_refs` which only detects junctions
/// within a single PBF block. Nodes in `global_shared` that appear in a way's
/// node refs are added to `preserve_node_refs` so DP simplification pins them.
#[cfg(test)]
#[allow(dead_code)]
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

pub(super) struct WayTaskResult {
    pub(super) chunk_paths: Vec<std::path::PathBuf>,
    pub(super) count: u64,
    pub(super) sink: RecordSink,
    pub(super) fanout: FanoutStats,
    pub(super) way_puts: Vec<(i64, Vec<(i32, i32)>)>,
}

pub(super) struct WayAcc {
    pub(super) sink: RecordSink,
    pub(super) bytes: usize,
    pub(super) chunk_paths: Vec<std::path::PathBuf>,
    pub(super) count: u64,
    pub(super) fanout: FanoutStats,
    pub(super) way_puts: Vec<(i64, Vec<(i32, i32)>)>,
    pub(super) merc: Vec<Point>,
    pub(super) point_emit: PointEmitScratch,
    pub(super) line_emit: LineEmitScratch,
    pub(super) polygon_emit: PolygonEmitScratch,
    pub(super) compression: crate::sort::ChunkCompression,
}

impl WayAcc {
    pub(super) fn new(compression: crate::sort::ChunkCompression) -> Self {
        Self {
            sink: RecordSink::new(),
            bytes: 0,
            chunk_paths: Vec::new(),
            count: 0,
            fanout: FanoutStats::new(),
            way_puts: Vec::new(),
            merc: Vec::new(),
            point_emit: PointEmitScratch::new(),
            line_emit: LineEmitScratch::new(),
            polygon_emit: PolygonEmitScratch::new(),
            compression,
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
        let paths = crate::sort::write_partitioned_payload_chunks(
            &mut self.sink.records,
            &self.sink.payload,
            chunk_dir,
            chunk_id,
            self.compression,
        )
        .expect("way chunk write failed");
        self.chunk_paths.extend(paths);
        self.count += self.sink.records.len() as u64;
        self.sink.clear_payload();
        self.bytes = 0;
    }

    pub(super) fn finish(self) -> WayTaskResult {
        WayTaskResult {
            chunk_paths: self.chunk_paths,
            count: self.count,
            sink: self.sink,
            fanout: self.fanout,
            way_puts: self.way_puts,
        }
    }
}

#[hotpath::measure]
pub(super) fn drain_way_task_result(
    result: WayTaskResult,
    way_index: &mut WayIndex,
    sort_writer: &mut SortWriter,
    fanout: &mut FanoutStats,
) -> u64 {
    for (way_id, coords_e7) in result.way_puts {
        way_index.put(way_id, &coords_e7);
    }
    sort_writer.adopt_chunk_files(result.chunk_paths);
    sort_writer.merge_tally(&result.sink.tally);
    fanout.merge(&result.fanout);
    let count = result.count + result.sink.records.len() as u64;
    for (key, off, len) in result.sink.records {
        sort_writer
            .push_untracked(SortRecord {
                key,
                data: result.sink.payload[off..off + len].into(),
            })
            .expect("sort push failed");
    }
    count
}

fn harvest_way_cap_events(acc: &mut WayAcc) {
    for &(idx, tiles, oid) in &acc.polygon_emit.cap_events {
        let layer = idx as usize / 15;
        let zoom = idx as usize % 15;
        acc.fanout.record_cap(layer, zoom, tiles, oid);
    }
    // `emit_polygon_feature` only clears cap_events when it runs, so a following
    // non-polygon (or non-matching) way would re-harvest this way's events. The
    // old thread-local path used `std::mem::take` per way; reproduce that
    // consume-once semantics by clearing here.
    acc.polygon_emit.cap_events.clear();
}

/// Process a raw way into a task-local arena.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn process_planned_way_into(
    way: &Way<'_>,
    plan: &WayPlan,
    is_member: bool,
    node_reader: Option<&NodeStoreReader>,
    min_zoom: u8,
    max_zoom: u8,
    seam_reconcile_layers: &[u8],
    deferral_stats: &DeferralStats,
    missing_ref_stats: &MissingRefStatsAtomic,
    fanout_caps: &[u32],
    polygon_simplify_factor: f64,
    acc: &mut WayAcc,
) {
    let tags_ref: Vec<(&str, &str)> = way.tags().collect();
    if tags_ref.is_empty() && !is_member {
        return;
    }
    let tag_helper = Tags(&tags_ref);
    // Members are always resolved (a relation reads their geometry back), so the
    // both-geom pre-filter is only worth computing for non-members, where it
    // gates the early return. Computing it for members would run two
    // `match_element` passes whose result is never inspected.
    if !is_member {
        let possible_feature = !shortbread::match_element(&tag_helper, OsmGeomType::ClosedWay)
            .is_empty()
            || !shortbread::match_element(&tag_helper, OsmGeomType::OpenWay).is_empty();
        if !possible_feature {
            return;
        }
    }

    let (coords_e7, resolved_node_refs): (Vec<(i32, i32)>, Vec<i64>) = if let Some(nr) = node_reader
    {
        let mut missing_refs: usize = 0;
        let mut resolved: Vec<(i32, i32)> = Vec::with_capacity(plan.node_refs.len());
        let mut resolved_refs: Vec<i64> = Vec::with_capacity(plan.node_refs.len());
        for &id in &plan.node_refs {
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
        (
            way.node_locations()
                .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon()))
                .collect(),
            plan.node_refs.clone(),
        )
    };

    if coords_e7.is_empty() {
        return;
    }
    if is_member {
        acc.way_puts.push((plan.way_id, coords_e7.clone()));
    }

    let is_closed = coords_e7.len() >= 4 && coords_e7.first() == coords_e7.last();
    let geom_type = if is_closed {
        OsmGeomType::ClosedWay
    } else {
        OsmGeomType::OpenWay
    };
    let mut matches = shortbread::match_element(&tag_helper, geom_type);

    if matches.is_empty() {
        return;
    }

    #[allow(clippy::cast_sign_loss)]
    let osm_id = plan.way_id as u64;
    let before = acc.sink.records.len();
    let mut preserve_vertex_mask: Vec<bool> = vec![false; coords_e7.len()];
    if !plan.preserve_node_refs.is_empty() {
        const PRESERVE_LINEAR_SCAN_MAX: usize = 8;
        if plan.preserve_node_refs.len() <= PRESERVE_LINEAR_SCAN_MAX {
            for (i, node_id) in resolved_node_refs.iter().enumerate() {
                if plan.preserve_node_refs.contains(node_id) {
                    preserve_vertex_mask[i] = true;
                }
            }
        } else {
            let preserve_nodes: FxHashSet<i64> = plan.preserve_node_refs.iter().copied().collect();
            for (i, node_id) in resolved_node_refs.iter().enumerate() {
                if preserve_nodes.contains(node_id) {
                    preserve_vertex_mask[i] = true;
                }
            }
        }
    }

    acc.merc.clear();
    acc.merc.extend(
        coords_e7
            .iter()
            .map(|&(lat, lon)| geometry::project_e7(lat, lon)),
    );
    let _ = unwrap_antimeridian_path(&mut acc.merc, is_closed);

    {
        let merc = acc.merc.as_slice();
        let merc_bbox_val = merc_bbox(merc);

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
                        &mut acc.sink,
                        &mut acc.point_emit,
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
                                &mut acc.sink,
                                &mut acc.line_emit,
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
                                &mut acc.sink,
                                &mut acc.line_emit,
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
                                &mut acc.sink,
                                &mut acc.polygon_emit,
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
                                &mut acc.sink,
                                &mut acc.polygon_emit,
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
    }
    record_fanout_from_payload_records(&acc.sink.records[before..], &mut acc.fanout);
    harvest_way_cap_events(acc);
    acc.bytes = acc.sink.bytes();
}

#[cfg(test)]
mod shared_node_helper_tests {
    use super::{FxHashSet, preserve_refs_for_way, shared_node_counts};

    // The block-local counting + scan-slice behaviour is exercised through
    // `annotate_block_shared_node_refs` in pipeline_tests.rs, which now delegates
    // to the same helpers as the production `build_way_plans`. These tests cover
    // the one branch `build_way_plans` adds on top: the `global_shared` union,
    // which the RawWay wrapper (called with `None`) cannot reach.

    #[test]
    fn global_shared_pins_a_non_block_local_node() {
        // node 20 appears in exactly one way here, so block-local counting alone
        // never pins it; the global cross-block set must force the pin.
        let refs = [10_i64, 20, 30];
        let counts = shared_node_counts([refs.as_slice()].into_iter());

        let without_global = preserve_refs_for_way(&refs, &counts, None);
        assert!(
            without_global.is_empty(),
            "block-local alone must not pin a singly-occurring node"
        );

        let global: FxHashSet<i64> = [20].into_iter().collect();
        let with_global = preserve_refs_for_way(&refs, &counts, Some(&global));
        assert_eq!(
            with_global,
            vec![20],
            "a node in the global-shared set must be pinned even if block-local count is 1"
        );
    }

    #[test]
    fn global_shared_never_pins_the_closing_dup_vertex() {
        // The closing vertex is excluded from the scan slice, so a closed ring's
        // node is still reachable via its first occurrence but the trailing
        // duplicate must not produce a second entry.
        let refs = [1_i64, 2, 3, 4, 1];
        let counts = shared_node_counts([refs.as_slice()].into_iter());
        let global: FxHashSet<i64> = [1].into_iter().collect();
        let preserve = preserve_refs_for_way(&refs, &counts, Some(&global));
        assert_eq!(
            preserve,
            vec![1],
            "closing node pinned once via its leading occurrence, not duplicated"
        );
    }
}
