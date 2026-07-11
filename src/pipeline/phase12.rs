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
    LineEmitScratch, PointEmitScratch, PolygonEmitScratch, RecordSink, RecordTally,
    antimeridian_shifts_for_bbox, emit_line_feature, emit_point_or_centroid, emit_polygon_feature,
    enrich_polygon_matches, push_sort_record, unwrap_antimeridian_path,
};
use super::relations::process_relation_blocks;
use super::stats::{
    DeferralStats, FanoutStats, MissingRefStatsAtomic, Phase12Stats,
    record_fanout_from_payload_records,
};
use super::{PipelineError, SORT_CHUNKS_DIR, TilegenConfig, current_rss_kb};

/// A way block plus its optional injected per-blob membership bitmap.
struct WayBlock {
    block: PrimitiveBlock,
    members: Option<Box<[u8]>>,
}

type DecodedWayBlock = Result<(PrimitiveBlock, Option<Box<[u8]>>), PipelineError>;

/// Where membership information is supplied for this run.
enum MemberSource {
    Injected,
    Plan(std::sync::Arc<RelationPlan>),
}

/// Per-block membership input used when constructing way plans.
pub(super) enum MembersForBlock<'a> {
    Bitmap(&'a [u8]),
    Set(&'a FxHashSet<i64>),
}

/// Injected-enrichment features declared by the PBF header.
struct InjectedFeatures {
    members: bool,
    pins: bool,
}

/// Where DP pins come from for this run.
#[derive(Clone, Copy)]
pub(super) enum PinSource {
    /// Header declares pbfhogg.SharedNodePins-v1: read Way field 20.
    Injected,
    /// Count shared refs within each primitive block.
    BlockLocal,
}

fn detect_injected_features(
    has_members: bool,
    has_pins: bool,
    has_locations: bool,
) -> Result<InjectedFeatures, PipelineError> {
    if (has_members || has_pins) && !has_locations {
        return Err(PipelineError(
            "injected WayMembers-v1 or SharedNodePins-v1 requires LocationsOnWays".to_string(),
        ));
    }
    Ok(InjectedFeatures {
        members: has_members,
        pins: has_pins,
    })
}

fn validate_and_take_members(
    members: Option<(&[u8], u32)>,
    actual_way_count: usize,
) -> Result<Box<[u8]>, PipelineError> {
    let (bitmap, encoded_count) = members.ok_or_else(|| {
        PipelineError("injected way-members bitmap missing or malformed".to_string())
    })?;
    let actual_way_count_u64 = u64::try_from(actual_way_count)
        .map_err(|_| PipelineError("decoded way count does not fit in u64".to_string()))?;
    if u64::from(encoded_count) != actual_way_count_u64 {
        return Err(PipelineError(format!(
            "injected way-members count mismatch: encoded {encoded_count}, decoded {actual_way_count}"
        )));
    }
    let expected_len = actual_way_count.div_ceil(8);
    if bitmap.len() != expected_len {
        return Err(PipelineError(format!(
            "injected way-members bitmap length mismatch: got {}, expected {expected_len}",
            bitmap.len()
        )));
    }
    Ok(bitmap.into())
}

fn member_bit(bitmap: &[u8], i: usize) -> bool {
    let byte = bitmap
        .get(i / 8)
        .expect("validated way-members bitmap shorter than decoded way count");
    byte & (1 << (i % 8)) != 0
}

fn validate_way_pin_bitmaps(block: &PrimitiveBlock) -> Result<(), PipelineError> {
    for element in block.elements() {
        let Element::Way(way) = element else {
            continue;
        };
        if let Some(bitmap) = way.shared_node_pins() {
            let ref_count = way.refs().count();
            let expected_len = ref_count.div_ceil(8);
            if bitmap.len() != expected_len {
                return Err(PipelineError(format!(
                    "injected shared-node pins bitmap length mismatch for way {}: got {}, expected {expected_len}",
                    way.id(),
                    bitmap.len()
                )));
            }
        }
    }
    Ok(())
}

fn fill_mask_from_pin_bitmap(bitmap: &[u8], mask: &mut [bool]) {
    for (i, pinned) in mask.iter_mut().enumerate() {
        *pinned = bitmap[i / 8] & (1 << (i % 8)) != 0;
    }
}

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
/// Relation blocks buffer at most this many decompressed bytes in RAM; past
/// the cap the tail re-reads relation blobs from the PBF instead (planet-scale
/// inputs hold several GB of relation blocks - an input-scaled stock the
/// 30 GB RAM ledger cannot absorb).
pub(super) const REL_BLOCKS_BUFFER_CAP: usize = 1024 * 1024 * 1024; // 1 GB

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
// Visible only without the hotpath feature (the measure macro's wrapping
// masks it under --all-features).
#[allow(clippy::cognitive_complexity)]
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
    let header = reader.header();
    let injected = detect_injected_features(
        header.has_way_members_v1(),
        header.has_shared_node_pins_v1(),
        header.has_locations_on_ways(),
    )?;
    if injected.members {
        eprintln!("  WayMembers-v1 detected - using injected membership");
    }
    let pin_source = if injected.pins {
        eprintln!("  SharedNodePins-v1 detected - using injected pins");
        PinSource::Injected
    } else {
        PinSource::BlockLocal
    };
    let locations_on_ways = config.locations_on_ways || header.has_locations_on_ways();

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
    let mut block_tx: Option<std::sync::mpsc::SyncSender<WayBlock>> = None;
    // Shared chunk-number allocator + spill coalescer, live for the whole
    // phase: the node worker, way tasks, drain writer, and relation tail all
    // produce chunks into the same directory concurrently. The counter is
    // attached to sort_writer here and detached (resyncing chunk_count) after
    // the relation tail adopts the coalescer's paths.
    let shared_chunk_id = std::sync::Arc::new(AtomicUsize::new(
        sort_writer
            .as_ref()
            .expect("sort_writer taken before phase12 read")
            .chunk_count(),
    ));
    sort_writer
        .as_mut()
        .expect("sort_writer taken before phase12 read")
        .attach_chunk_counter(std::sync::Arc::clone(&shared_chunk_id));
    let spill = std::sync::Arc::new(crate::sort::SpillCoalescer::new(
        config.tmp_dir.join(SORT_CHUNKS_DIR),
        std::sync::Arc::clone(&shared_chunk_id),
        sort_chunk_budget,
        config.compress_sort_chunks,
    ));
    let mut worker_handle: Option<std::thread::JoinHandle<()>> = None;
    // Drain thread owns way_index + sort_writer during way phase, returns them when done.
    let mut drain_handle: Option<
        std::thread::JoinHandle<(WayIndex, SortWriter, u64, FanoutStats)>,
    > = None;

    // Buffer relation blocks - processed after all PBF blocks are consumed so that
    // late way blocks (common in locations-on-ways PBFs) don't hit a finalized way_index.
    let mut relation_blocks: Vec<PrimitiveBlock> = Vec::new();
    let mut relation_blocks_spilled = false;
    // ELIVAGAR_REL_BLOCKS_CAP overrides the buffer cap (bytes) - debug/test
    // hook to force the re-read path on small extracts.
    let rel_blocks_buffer_cap: usize = std::env::var("ELIVAGAR_REL_BLOCKS_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(REL_BLOCKS_BUFFER_CAP);

    // High-water-mark counters for in-flight memory tracking.
    let way_hwm = std::sync::Arc::new(AtomicUsize::new(0));
    // Ways counted task-side (from plans.len()) so the ordered consumer
    // never re-parses way blocks.
    let way_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let way_members_marked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let way_pins_marked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    // RAM-ledger sizes of the planet-scaling structures phase12 holds:
    // buffered relation blocks (decompressed bytes), the relation plan's
    // member-way set.
    let mut relation_blocks_bytes: usize = 0;
    let mut relation_plan_needed_ways: usize = 0;
    let mut relation_plan_superset_ways: usize = 0;

    // Reusable buffer hoisted out of the PBF closure to avoid per-element
    // allocation (~200M allocs at planet scale). Cleared each iteration.
    // tags_vec cannot be hoisted: it holds &str references into PBF elements
    // that don't outlive the closure body (mutable reference invariance).
    let relation_plan_pbf_path = config.pbf_path.clone();
    let mut relation_plan_handle: Option<
        std::thread::JoinHandle<Result<RelationPlan, PipelineError>>,
    > = if injected.members {
        None
    } else {
        Some(std::thread::spawn(move || {
            prepass_relation_plan(&relation_plan_pbf_path, decode_threads)
        }))
    };

    // Node worker: owns the node store during the node phase and processes
    // node blocks off the consumer (8s of serial consumer time on germany
    // locations, all of it stalling decode workers). Records flow to the
    // shared spill coalescer, so the worker never owns sort_writer. On the
    // raw path (ordered source, node store) it is joined at the first way
    // block, which also preserves sequential store-put order; on the
    // locations path (unordered source, no store) it runs for the whole read
    // and is joined after the drain returns.
    let mut node_worker_tx: Option<std::sync::mpsc::SyncSender<PrimitiveBlock>> = None;
    let mut node_worker_handles: Vec<std::thread::JoinHandle<NodeWorkerState>> = Vec::new();
    let mut node_worker_joined = false;
    // Node-worker bookkeeping merged at join time; tally applied to
    // sort_writer wherever it lives at that point.
    let mut node_tally: Option<RecordTally> = None;

    macro_rules! join_node_worker {
        () => {{
            if let Some(tx) = node_worker_tx.take() {
                drop(tx);
                let _wait = wait_span(&WAIT.node_worker_join);
                for handle in node_worker_handles.drain(..) {
                    let st = handle.join().expect("node worker thread panicked");
                    if st.node_store.is_some() {
                        node_store_opt = st.node_store;
                    }
                    node_count += st.node_count;
                    features_emitted += st.features_emitted;
                    match node_tally.as_mut() {
                        Some(tally) => tally.merge(&st.tally),
                        None => node_tally = Some(st.tally),
                    }
                    min_lat_e7 = min_lat_e7.min(st.min_lat_e7);
                    max_lat_e7 = max_lat_e7.max(st.max_lat_e7);
                    min_lon_e7 = min_lon_e7.min(st.min_lon_e7);
                    max_lon_e7 = max_lon_e7.max(st.max_lon_e7);
                    min_lon_shifted_e7 = min_lon_shifted_e7.min(st.min_lon_shifted_e7);
                    max_lon_shifted_e7 = max_lon_shifted_e7.max(st.max_lon_shifted_e7);
                }
                node_worker_joined = true;
            }
        }};
    }

    // Block routing shared by both sources. Expanded as a macro (not a
    // closure) because the body mutably borrows a dozen locals and spawns
    // threads that capture others.
    macro_rules! route_block {
        ($block:expr, $members:expr) => {{
        let block: PrimitiveBlock = $block;
        let members: Option<Box<[u8]>> = $members;
        // Classify block by reading first wire tag byte per group -
        // no element decoding. Sorted PBFs have single-type blocks.
        match block.block_type() {
            BlockType::DenseNodes | BlockType::Nodes => {
                // Node block - forward to the node worker; spawn it lazily on
                // the first one. The consumer only classifies and sends.
                if node_worker_tx.is_none() {
                    // Raw path only: a node block after the way phase would
                    // respawn a worker whose store was already consumed and
                    // silently lose puts. The ordered source makes this
                    // impossible for sorted PBFs; make violations loud.
                    assert!(
                        !node_worker_joined || locations_on_ways,
                        "node block after way phase on the raw path"
                    );
                    // The raw path is pinned to ONE worker: the sorted node
                    // store requires sequential put order. Locations mode has
                    // no store and an unordered source - tagged-node emission
                    // and extent tracking are order-free, so the blocks fan
                    // out to a small worker pool (the single worker was the
                    // node-phase rate limiter: 16.6s serial on NA locations,
                    // node_block_send wait 16.5s from the consumer side).
                    let worker_count = if locations_on_ways {
                        (config.threads / 4).clamp(2, 6)
                    } else {
                        1
                    };
                    let (ntx, nrx) = std::sync::mpsc::sync_channel::<PrimitiveBlock>(16);
                    let shared_rx = std::sync::Arc::new(std::sync::Mutex::new(nrx));
                    let mut ns = node_store_opt.take();
                    for _ in 0..worker_count {
                        let rx = std::sync::Arc::clone(&shared_rx);
                        let sp = std::sync::Arc::clone(&spill);
                        let ns_taken = ns.take();
                        let mz = min_z;
                        let xz = max_z;
                        node_worker_handles.push(std::thread::spawn(move || {
                            run_node_worker(&rx, ns_taken, &sp, mz, xz)
                        }));
                    }
                    node_worker_tx = Some(ntx);
                }
                let _wait = wait_span(&WAIT.node_block_send);
                node_worker_tx
                    .as_ref()
                    .expect("node worker not initialized")
                    .send(block)
                    .expect("node worker thread panicked");
            }
            BlockType::Ways => {
                // Way block - send entire block to worker thread. Ways are
                // counted task-side from plans.len(): re-parsing the block
                // here cost 5.7s of ordered-consumer serial time on germany
                // locations (phase12_way_count_ns), all of it stalling the
                // decode workers behind pipeline_decoded_send.

                // Raw path: the node phase ends at the first way block -
                // reclaim the node store before building its reader. On the
                // locations path the node worker keeps running (no store, no
                // ordering requirement).
                if !locations_on_ways {
                    join_node_worker!();
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

                    let (btx, brx) = std::sync::mpsc::sync_channel::<WayBlock>(1);
                    // Capacity must be at least the in-flight task ceiling: rayon tasks block on send()
                    // while holding a rayon thread. If capacity < inflight tasks,
                    // blocked senders tie up all rayon threads → worker (which runs
                    // inside rayon::in_place_scope) can't make progress → deadlock.
                    let max_inflight = config.threads.max(8);
                    let (rtx, rrx) = std::sync::mpsc::sync_channel::<WayTaskResult>(max_inflight);
                    let nr_clone = nr.clone();
                    let way_hwm_clone = std::sync::Arc::clone(&way_hwm);
                    let way_counter_clone = std::sync::Arc::clone(&way_counter);
                    let way_members_marked_clone = std::sync::Arc::clone(&way_members_marked);
                    let way_pins_marked_clone = std::sync::Arc::clone(&way_pins_marked);
                    let missing_ref_stats_clone = std::sync::Arc::clone(&missing_ref_stats);
                    let deferral_stats_clone = std::sync::Arc::clone(&deferral_stats);
                    let mz = min_z;
                    let xz = max_z;
                    let srl = config.seam_reconcile_layers;
                    let fcs = config.fanout_caps;
                    let psf = config.polygon_simplify_factor;
                    let way_chunk_size = sort_chunk_budget;
                    let worker_spill = std::sync::Arc::clone(&spill);
                    // The fallback relation plan is first needed here. Its join
                    // blocks the ordered consumer, so it is stall time.
                    let prepass_join_guard = wait_span(&WAIT.prepass_join);
                    let member_source = if injected.members {
                        MemberSource::Injected
                    } else {
                        MemberSource::Plan(std::sync::Arc::new(
                            relation_plan_handle
                                .take()
                                .expect("relation prepass missing")
                                .join()
                                .map_err(|_| {
                                    PipelineError("relation prepass thread panicked".to_string())
                                })??,
                        ))
                    };
                    drop(prepass_join_guard);
                    if let MemberSource::Plan(plan) = &member_source {
                        relation_plan_needed_ways = plan.needed_ways.len();
                        relation_plan_superset_ways = plan.superset_ways_count;
                    }
                    // Multi-block overlap: rayon::scope allows multiple blocks' ways
                    // in the pool simultaneously. Byte-budgeted in-flight control
                    // limits total estimated memory, with a count ceiling as safety net.
                    const WAY_OUTPUT_MULTIPLIER: usize = 10;
                    // ELIVAGAR_WAY_BUDGET (bytes) outranks the flag: brokkr's
                    // tilegen wrapper has no --way-budget passthrough, and the
                    // budget is under active A/B (way_budget wait 93.4s
                    // cumulative on NA locations at the 768M default).
                    let way_budget = std::env::var("ELIVAGAR_WAY_BUDGET")
                        .ok()
                        .and_then(|v| v.parse::<usize>().ok())
                        .filter(|&v| v > 0)
                        .unwrap_or(if config.way_inflight_budget > 0 {
                            config.way_inflight_budget
                        } else if locations_on_ways {
                            DEFAULT_WAY_BUDGET_LOCATIONS
                        } else {
                            DEFAULT_WAY_BUDGET
                        });
                    worker_handle = Some(std::thread::spawn(move || {
                        // Take refs outside loop - Copy into each move closure,
                        // avoids Arc::clone per spawn.
                        let nr_ref: Option<&NodeStoreReader> = nr_clone.as_deref();
                        let mr_ref = &*missing_ref_stats_clone;
                        let ds_ref = &*deferral_stats_clone;
                        let member_source_ref = &member_source;
                        let spill_ref = &*worker_spill;
                        // Byte-budgeted throttle: (count, estimated_bytes).
                        // Condvar wakes dispatcher when a task completes.
                        let inflight = std::sync::Mutex::new((0usize, 0usize));
                        let inflight_cvar = std::sync::Condvar::new();
                        let inflight_ref = &inflight;
                        let cvar_ref = &inflight_cvar;
                        let way_counter_ref = &*way_counter_clone;
                        let way_members_marked_ref = &*way_members_marked_clone;
                        let way_pins_marked_ref = &*way_pins_marked_clone;
                        // Pool of accumulators shared across block tasks. Accs
                        // live for the whole way phase (not one block), so the
                        // bulk of the record volume drains through the shared
                        // spill coalescer instead of funneling through the drain
                        // thread's serial sort_writer pushes - measured on
                        // germany locations: 12.3 GB of records through one
                        // thread, 141s of tasks blocked on way_result_send
                        // behind it. The drain keeps way_index ownership;
                        // per-block results carry only the way_puts. The flush
                        // threshold is small because a flush is now a memcpy
                        // into the coalescer, not a chunk-file write; buffered
                        // ceiling is one acc per concurrently running task.
                        const WAY_ACC_FLUSH_BYTES: usize = 8 * 1024 * 1024;
                        let acc_flush_bytes = way_chunk_size.min(WAY_ACC_FLUSH_BYTES);
                        let acc_pool: std::sync::Mutex<Vec<WayAcc>> =
                            std::sync::Mutex::new(Vec::new());
                        let acc_pool_ref = &acc_pool;
                        rayon::in_place_scope(|s| {
                            while let Ok(way_block) = brx.recv() {
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
                                let block_cost = way_block.block.decompressed_size() * WAY_OUTPUT_MULTIPLIER;
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
                                let way_hwm_task = std::sync::Arc::clone(&way_hwm_clone);
                                #[allow(clippy::let_underscore_must_use)]
                                s.spawn(move |_| {
                                    // Now summed across rayon workers, not a serial
                                    // stage: read phase12_plan_build_ns as thread-time.
                                    let plan_busy = wait_span(&BUSY.phase12_plan_build);
                                    let members = match member_source_ref {
                                        MemberSource::Injected => MembersForBlock::Bitmap(
                                            way_block.members.as_deref().expect(
                                                "way block without members bitmap on injected path",
                                            ),
                                        ),
                                        MemberSource::Plan(plan) => {
                                            MembersForBlock::Set(&plan.needed_ways)
                                        }
                                    };
                                    let (plans, marked) =
                                        build_way_plans(&way_block.block, &members, pin_source);
                                    drop(plan_busy);
                                    way_counter_ref
                                        .fetch_add(plans.len() as u64, Ordering::Relaxed);
                                    way_members_marked_ref.fetch_add(marked, Ordering::Relaxed);
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
                                        .unwrap_or_else(WayAcc::new);
                                    let mut plans = plans.into_iter();
                                    for element in way_block.block.elements() {
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
                                        let pins_marked = process_planned_way_into(
                                            &way, &plan, nr_ref, mz, xz, &srl, ds_ref, mr_ref,
                                            &fcs, psf, pin_source, &mut acc,
                                        );
                                        way_pins_marked_ref
                                            .fetch_add(pins_marked, Ordering::Relaxed);
                                        if acc.bytes >= acc_flush_bytes {
                                            acc.flush(spill_ref);
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
                        // sw arrives with the shared chunk counter already
                        // attached (phase12 setup) - its own flushes and every
                        // concurrent producer allocate from the same Arc.
                        let mut count: u64 = 0;
                        let mut fanout = FanoutStats::new();
                        while let Ok(results) = rrx.recv() {
                            let _busy = wait_span(&BUSY.phase12_drain);
                            count += drain_way_task_result(results, &mut wi, &mut sw, &mut fanout);
                            ds_drain.check_budgets(&srl_drain);
                        }
                        // The shared chunk counter stays attached: the spill
                        // coalescer keeps allocating from the same Arc through
                        // the relation tail. Detached (and chunk_count resynced)
                        // after the tail adopts the coalescer's paths.
                        (wi, sw, count, fanout)
                    }));

                    block_tx = Some(btx);
                }

                // send() blocks if worker is still processing previous block (backpressure)
                let _wait = wait_span(&WAIT.way_block_send);
                block_tx
                    .as_ref()
                    .expect("worker not initialized")
                    .send(WayBlock { block, members })
                    .expect("worker thread panicked");
            }
            BlockType::Relations => {
                // Buffer relation blocks - defer processing until all PBF blocks
                // are consumed. Locations-on-ways PBFs can have way blocks after
                // relation blocks; processing relations inline would finalize the
                // way_index too early.
                //
                // The buffer is byte-capped: this stock is input-scaled (planet
                // holds several GB of decompressed relation blocks) and would
                // eat the planet RAM budget. Past the cap, drop everything
                // buffered and re-read relation blobs from the PBF at the tail
                // via BlobFilter::only_relations (indexed PBFs skip-read).
                relation_blocks_bytes += block.decompressed_size();
                if !relation_blocks_spilled {
                    if relation_blocks_bytes > rel_blocks_buffer_cap {
                        relation_blocks_spilled = true;
                        relation_blocks = Vec::new();
                        eprintln!(
                            "  Relation blocks exceed {} MB buffer cap - re-reading at tail",
                            rel_blocks_buffer_cap / (1024 * 1024)
                        );
                    } else {
                        relation_blocks.push(block);
                    }
                }
            }
            BlockType::Empty | BlockType::Mixed => {}
        }
        }};
    }

    if locations_on_ways {
        // Elivagar-owned bounded read: see UnorderedBlockSource. Everything
        // downstream of this loop is order-free in locations mode.
        let source = UnorderedBlockSource::spawn(
            &config.pbf_path,
            decode_threads,
            injected.members,
            matches!(pin_source, PinSource::Injected),
        )?;
        drop(reader);
        loop {
            let item = {
                let _wait = wait_span(&WAIT.read_decoded_recv);
                source.rx.recv()
            };
            let Ok(block_result) = item else {
                break; // every sender done
            };
            let (block, members) = block_result?;
            route_block!(block, members);
        }
        source.join();
    } else {
        for block_result in reader.into_blocks_pipelined() {
            let block = block_result.map_err(|e| PipelineError(format!("PBF read failed: {e}")))?;
            route_block!(block, None);
        }
    }

    // Close the node worker's channel so it drains and exits. On the raw
    // path it was already joined at the first way block; on the locations
    // path it ran for the whole read and is joined below.
    join_node_worker!();
    // The joined flag guards node-after-way respawns inside the loop; this
    // final expansion's write is intentionally unread.
    let _ = node_worker_joined;
    // Nothing after the way phase reads the node store - release it.
    drop(node_store_opt.take());

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

    // Apply the node worker's record tally now that sort_writer is back from
    // the drain (locations path joins the worker after the read loop; the
    // raw path merged nothing here because sort_writer was present at its
    // first-way-block join - the tally rides the same Option either way).
    if let Some(tally) = node_tally.take() {
        sort_writer
            .as_mut()
            .expect("sort_writer not returned from drain")
            .merge_tally(&tally);
    }

    // Finalize way index after all way blocks are processed.
    if let Some(ref mut wi) = way_index {
        wi.finish_writing().expect("failed to finalize way index");
    }
    let way_count = way_counter.load(Ordering::Relaxed);
    eprintln!("  Ways: {way_count}, Features so far: {features_emitted}");
    eprintln!("  Way index finalized, processing relations...");

    let relation_blocks_buffered = relation_blocks.len();
    // Relation source: the in-RAM buffer, or a filtered PBF re-read when the
    // buffer cap tripped (input-scaled stock; see REL_BLOCKS_BUFFER_CAP).
    let rel_blocks_source: Box<dyn Iterator<Item = PrimitiveBlock> + Send> =
        if relation_blocks_spilled {
            let reader = ElementReader::from_path(&config.pbf_path)
                .map_err(|e| PipelineError(format!("relation re-read: failed to open PBF: {e}")))?
                .with_blob_filter(BlobFilter::only_relations())
                .decode_threads(decode_threads);
            Box::new(reader.into_blocks_pipelined().map(|r| {
                // Panic: mid-tail I/O failure is unrecoverable (same policy
                // as sort pushes).
                r.expect("relation re-read: PBF read failed")
            }))
        } else {
            Box::new(std::mem::take(&mut relation_blocks).into_iter())
        };
    // The relation tail appends to the same phase-wide spill coalescer; the
    // shared chunk counter is still attached to sort_writer.
    // Process relation blocks. Tail of phase12: one streamed parallel pass
    // over all relations (prepare interleaved with emit, a single end
    // barrier). The span deliberately includes the rayon fan-out - it is
    // wall time appended to the phase either way.
    let relation_tail_busy = wait_span(&BUSY.phase12_relation_tail);
    let relation_tail = process_relation_blocks(
        rel_blocks_source,
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
        &spill,
        &mut fanout_stats,
        &config.fanout_caps,
        config.polygon_simplify_factor,
    );
    rel_count += relation_tail.rel_count;
    features_emitted += relation_tail.features;
    let max_rel_inflight_bytes = relation_tail.max_inflight_bytes;
    deferral_stats.check_budgets(&config.seam_reconcile_layers);
    drop(relation_tail_busy);
    // Way-phase and relation-tail records all flowed through the coalescer;
    // write its residual, adopt every coalesced chunk, and resync the chunk
    // counter now that the last concurrent producer is done.
    {
        let sw = sort_writer
            .as_mut()
            .expect("sort_writer not returned from drain");
        sw.adopt_chunk_files(spill.finish());
        sw.detach_chunk_counter();
    }

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
        relation_blocks_spilled,
        relation_plan_needed_ways,
        relation_plan_superset_ways,
        way_members_marked: way_members_marked.load(Ordering::Relaxed),
        way_pins_marked: way_pins_marked.load(Ordering::Relaxed),
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
                    m.paint_rank,
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

/// Compute the preserve set for one way from block-local shared refs.
fn preserve_refs_for_way(node_refs: &[i64], counts: &FxHashMap<i64, u8>) -> Vec<i64> {
    let mut preserve: Vec<i64> = Vec::new();
    for &node_id in shared_scan_slice(node_refs) {
        let shared = counts.get(&node_id).is_some_and(|&c| c >= 2);
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
        w.preserve_node_refs = preserve_refs_for_way(&w.node_refs, &counts);
    }
}

/// Bounded, UNORDERED pipelined block source for the locations-on-ways read.
///
/// Replaces pbfhogg's `into_blocks_pipelined` for the production input shape.
/// That reader's decode fan-out holds unbounded raw blobs in its pool queue
/// (the dispatcher spawns per blob without backpressure, so effective
/// read-ahead is the whole file at NVMe rate) and unbounded decoded blocks in
/// its reorder window whenever one decode straggles - measured on the NA
/// locations re-baseline: 660-1134-blob reorder windows, a 20 GB RSS ramp in
/// the first 20s of the run. pbfhogg itself migrated its planet-scale
/// commands off that reader for the same pathology (see cat_filtered's notes
/// on cross-thread PrimitiveBlock retention, ~25 GB at planet, OOM at
/// 28.9 GB); this is elivagar's equivalent move, built on the public
/// BlobReader + Blob::to_primitiveblock surface.
///
/// Locations mode consumes every block order-free - there is no node store,
/// the external sort erases way emission order, and relation blocks are
/// buffered or re-read - so no reorder buffer exists AT ALL: raw blobs are
/// bounded by the raw channel, decoded blocks by the decoded channel, and
/// total in-flight is raw_cap + decoded_cap + decode workers, each bounded
/// and input-independent. Backpressure propagates to the reader thread: the
/// file is read at the rate the consumer absorbs it.
///
/// The raw path (node store, order-dependent) stays on pbfhogg's ordered
/// reader.
struct UnorderedBlockSource {
    rx: std::sync::mpsc::Receiver<DecodedWayBlock>,
    reader_handle: std::thread::JoinHandle<()>,
    decode_handles: Vec<std::thread::JoinHandle<()>>,
}

impl UnorderedBlockSource {
    fn spawn(
        path: &std::path::Path,
        decode_threads: usize,
        injected_members: bool,
        injected_pins: bool,
    ) -> Result<Self, PipelineError> {
        let raw_cap = (decode_threads * 2).max(8);
        let decoded_cap = (decode_threads * 2).max(8);
        let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<pbfhogg::Blob>(raw_cap);
        let raw_rx = std::sync::Arc::new(std::sync::Mutex::new(raw_rx));
        let (decoded_tx, decoded_rx) =
            std::sync::mpsc::sync_channel::<DecodedWayBlock>(decoded_cap);

        let mut reader = pbfhogg::BlobReader::from_path(path)
            .map_err(|e| PipelineError(format!("failed to open PBF: {e}")))?;
        reader.set_parse_waymembers(injected_members);

        let reader_err_tx = decoded_tx.clone();
        let reader_handle = std::thread::spawn(move || {
            for blob_result in reader {
                match blob_result {
                    Ok(blob) => {
                        if !matches!(blob.get_type(), pbfhogg::BlobType::OsmData) {
                            continue;
                        }
                        let _wait = wait_span(&WAIT.read_raw_send);
                        if raw_tx.send(blob).is_err() {
                            break; // consumer gone (error path); stop reading
                        }
                    }
                    Err(e) => {
                        drop(
                            reader_err_tx.send(Err(PipelineError(format!("PBF read failed: {e}")))),
                        );
                        break;
                    }
                }
            }
            // raw_tx drops: decode workers drain the channel and exit.
        });

        let mut decode_handles = Vec::with_capacity(decode_threads);
        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&raw_rx);
            let tx = decoded_tx.clone();
            decode_handles.push(std::thread::spawn(move || {
                loop {
                    let blob = {
                        let guard = rx.lock().expect("raw blob channel lock");
                        guard.recv()
                    };
                    let Ok(blob) = blob else {
                        break; // reader done and channel drained
                    };
                    let item = (|| {
                        let members = blob.way_members().zip(blob.way_member_count());
                        let block = blob
                            .to_primitiveblock()
                            .map_err(|e| PipelineError(format!("PBF decode failed: {e}")))?;
                        let members = if injected_members && block.block_type() == BlockType::Ways {
                            let actual_way_count = block
                                .elements()
                                .filter(|element| matches!(element, Element::Way(_)))
                                .count();
                            Some(validate_and_take_members(members, actual_way_count)?)
                        } else {
                            None
                        };
                        if injected_pins {
                            validate_way_pin_bitmaps(&block)?;
                        }
                        Ok((block, members))
                    })();
                    let _wait = wait_span(&WAIT.read_decoded_send);
                    if tx.send(item).is_err() {
                        break; // consumer gone
                    }
                }
            }));
        }
        drop(decoded_tx);

        Ok(Self {
            rx: decoded_rx,
            reader_handle,
            decode_handles,
        })
    }

    /// Join the source threads after the receiver has been drained (or on
    /// early error exit - dropping the receiver unblocks every sender).
    fn join(self) {
        drop(self.rx);
        self.reader_handle
            .join()
            .expect("blob reader thread panicked");
        for handle in self.decode_handles {
            handle.join().expect("blob decode thread panicked");
        }
    }
}

/// Everything the node worker owns during the node phase, handed back when
/// it is joined (first way block on the raw path; end of read otherwise).
pub(super) struct NodeWorkerState {
    pub(super) node_store: Option<NodeStore>,
    pub(super) node_count: u64,
    pub(super) features_emitted: u64,
    pub(super) tally: RecordTally,
    pub(super) min_lat_e7: i32,
    pub(super) max_lat_e7: i32,
    pub(super) min_lon_e7: i32,
    pub(super) max_lon_e7: i32,
    pub(super) min_lon_shifted_e7: i64,
    pub(super) max_lon_shifted_e7: i64,
}

/// Node-phase worker loop: store puts, tagged-node feature emission, and
/// data-extent tracking, fed whole blocks by the consumer through a shared
/// receiver. The raw path runs exactly ONE of these - the sorted node store
/// requires sequential put order (the raw path's ordered source guarantees
/// block order end to end). Locations mode runs a small pool: no store, an
/// unordered source, and per-worker sinks make node work order-free.
/// Records flow to the shared spill coalescer, so workers never own
/// sort_writer and can outlive the node phase - node blocks interleaved
/// with way blocks are fine when there is no node store.
fn run_node_worker(
    rx: &std::sync::Mutex<std::sync::mpsc::Receiver<PrimitiveBlock>>,
    mut node_store: Option<NodeStore>,
    spill: &crate::sort::SpillCoalescer,
    min_zoom: u8,
    max_zoom: u8,
) -> NodeWorkerState {
    const NODE_SINK_FLUSH_BYTES: usize = 8 * 1024 * 1024;
    let mut node_count: u64 = 0;
    let mut features_emitted: u64 = 0;
    let mut min_lat_e7: i32 = i32::MAX;
    let mut max_lat_e7: i32 = i32::MIN;
    let mut min_lon_e7: i32 = i32::MAX;
    let mut max_lon_e7: i32 = i32::MIN;
    let mut min_lon_shifted_e7: i64 = i64::MAX;
    let mut max_lon_shifted_e7: i64 = i64::MIN;
    let mut node_records: Vec<SortRecord> = Vec::new();
    let mut sink = RecordSink::new();

    // Macro to handle Node and DenseNode identically - both types expose the
    // same API (.id(), .decimicro_lat(), .decimicro_lon(), .tags()) but are
    // distinct types, so a generic function would not work without a trait.
    macro_rules! handle_node {
        ($node:expr) => {{
            node_count += 1;
            let lat_e7 = $node.decimicro_lat();
            let lon_e7 = $node.decimicro_lon();
            if let Some(ns) = node_store.as_mut() {
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
                    $node.id() as u64,
                    lat_e7,
                    lon_e7,
                    &tags_vec,
                    min_zoom,
                    max_zoom,
                    &mut node_records,
                );
                for r in node_records.drain(..) {
                    sink.tally.record(r.key, r.data.len());
                    sink.records.push((r.key, sink.payload.len(), r.data.len()));
                    sink.payload.extend_from_slice(&r.data);
                }
                features_emitted += n;
            }
        }};
    }

    loop {
        // Take the lock only to receive; blocks are processed lock-free so
        // pool siblings pull work concurrently.
        let received = {
            let guard = rx.lock().expect("node worker receiver lock");
            guard.recv()
        };
        let Ok(block) = received else { break };
        let _busy = wait_span(&BUSY.phase12_node_blocks);
        block.for_each_element(|element| match element {
            Element::DenseNode(node) => handle_node!(node),
            Element::Node(node) => handle_node!(node),
            _ => {}
        });
        if sink.bytes() >= NODE_SINK_FLUSH_BYTES {
            spill.append(&sink.records, &sink.payload);
            sink.clear_payload();
        }
    }
    spill.append(&sink.records, &sink.payload);
    sink.clear_payload();

    NodeWorkerState {
        node_store,
        node_count,
        features_emitted,
        tally: sink.tally,
        min_lat_e7,
        max_lat_e7,
        min_lon_e7,
        max_lon_e7,
        min_lon_shifted_e7,
        max_lon_shifted_e7,
    }
}

pub(super) struct WayPlan {
    pub(super) way_id: i64,
    pub(super) node_refs: Vec<i64>,
    pub(super) preserve_node_refs: Vec<i64>,
    /// Whether this way is a member of a shortbread-matched multipolygon/
    /// boundary relation. Resolved once here from the relation plan's
    /// `needed_ways` set, replacing the former per-way `contains` lookup at
    /// the `process_planned_way_into` call site - one place computes
    /// membership.
    pub(super) is_member: bool,
}

fn estimate_way_plans_bytes(plans: &[WayPlan]) -> usize {
    plans
        .iter()
        .map(|p| {
            std::mem::size_of::<WayPlan>() + p.node_refs.len() * 8 + p.preserve_node_refs.len() * 8
        })
        .sum()
}

pub(super) fn build_way_plans(
    block: &PrimitiveBlock,
    members: &MembersForBlock<'_>,
    pins: PinSource,
) -> (Vec<WayPlan>, u64) {
    let mut way_pos = 0usize;
    let mut marked = 0u64;
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
            let way_id = way.id();
            let is_member = match members {
                MembersForBlock::Bitmap(bitmap) => member_bit(bitmap, way_pos),
                MembersForBlock::Set(needed_ways) => needed_ways.contains(&way_id),
            };
            way_pos += 1;
            marked += u64::from(is_member);
            Some(WayPlan {
                way_id,
                node_refs: way.refs().collect(),
                preserve_node_refs: Vec::new(),
                // Membership resolved once, at plan build, from the relation
                // plan's needed_ways set - formerly a per-way `contains` lookup
                // at the process_planned_way_into call site.
                is_member,
            })
        })
        .collect();

    if matches!(pins, PinSource::BlockLocal) {
        let counts = shared_node_counts(plans.iter().map(|p| p.node_refs.as_slice()));
        for plan in &mut plans {
            plan.preserve_node_refs = preserve_refs_for_way(&plan.node_refs, &counts);
        }
    }
    (plans, marked)
}

pub(super) struct RelationPlan {
    pub(super) needed_ways: FxHashSet<i64>,
    /// Size of the union of `needed_ways` with the member ways of
    /// mp/boundary relations that FAIL the shortbread match - i.e. members
    /// of ALL type=multipolygon/boundary relations. This is exactly the set
    /// an enrichment-time producer with no shortbread knowledge would mark
    /// (superset semantics), so `superset_ways_count / needed_ways.len()`
    /// is the superset inflation factor. Count only; the set is transient.
    pub(super) superset_ways_count: usize,
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
    // Members of ALL mp/boundary relations, shortbread match or not - what
    // an enrichment-time superset bitmap would contain. Only the count is
    // kept; the set is dropped before the plan is returned.
    let mut superset_ways: FxHashSet<i64> = FxHashSet::default();
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
            let matched =
                !shortbread::match_element(&tag_helper, OsmGeomType::MultiPolygon).is_empty();
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
        superset_ways_count,
    })
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
    pub(super) count: u64,
    pub(super) sink: RecordSink,
    pub(super) fanout: FanoutStats,
    pub(super) way_puts: Vec<(i64, Vec<(i32, i32)>)>,
}

pub(super) struct WayAcc {
    pub(super) sink: RecordSink,
    pub(super) bytes: usize,
    pub(super) count: u64,
    pub(super) fanout: FanoutStats,
    pub(super) way_puts: Vec<(i64, Vec<(i32, i32)>)>,
    pub(super) merc: Vec<Point>,
    pub(super) point_emit: PointEmitScratch,
    pub(super) line_emit: LineEmitScratch,
    pub(super) polygon_emit: PolygonEmitScratch,
}

impl WayAcc {
    pub(super) fn new() -> Self {
        Self {
            sink: RecordSink::new(),
            bytes: 0,
            count: 0,
            fanout: FanoutStats::new(),
            way_puts: Vec::new(),
            merc: Vec::new(),
            point_emit: PointEmitScratch::new(),
            line_emit: LineEmitScratch::new(),
            polygon_emit: PolygonEmitScratch::new(),
        }
    }

    /// Hand the sink's records to the shared spill coalescer (a memcpy under
    /// its lock) and reset for reuse. Tally stays in the sink and is merged
    /// when the residual acc ships through the drain at end of stream.
    pub(super) fn flush(&mut self, spill: &crate::sort::SpillCoalescer) {
        if self.sink.records.is_empty() {
            return;
        }
        spill.append(&self.sink.records, &self.sink.payload);
        self.count += self.sink.records.len() as u64;
        self.sink.clear_payload();
        self.bytes = 0;
    }

    pub(super) fn finish(self) -> WayTaskResult {
        WayTaskResult {
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
    node_reader: Option<&NodeStoreReader>,
    min_zoom: u8,
    max_zoom: u8,
    seam_reconcile_layers: &[u8],
    deferral_stats: &DeferralStats,
    missing_ref_stats: &MissingRefStatsAtomic,
    fanout_caps: &[u32],
    polygon_simplify_factor: f64,
    pins: PinSource,
    acc: &mut WayAcc,
) -> u64 {
    let tags_ref: Vec<(&str, &str)> = way.tags().collect();
    if tags_ref.is_empty() && !plan.is_member {
        return 0;
    }
    let tag_helper = Tags(&tags_ref);
    // Members are always resolved (a relation reads their geometry back), so the
    // both-geom pre-filter is only worth computing for non-members, where it
    // gates the early return. Computing it for members would run two
    // `match_element` passes whose result is never inspected.
    if !plan.is_member {
        let possible_feature = !shortbread::match_element(&tag_helper, OsmGeomType::ClosedWay)
            .is_empty()
            || !shortbread::match_element(&tag_helper, OsmGeomType::OpenWay).is_empty();
        if !possible_feature {
            return 0;
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
        return 0;
    }
    if plan.is_member {
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
        return 0;
    }

    #[allow(clippy::cast_sign_loss)]
    let osm_id = plan.way_id as u64;
    let before = acc.sink.records.len();
    let mut preserve_vertex_mask: Vec<bool> = vec![false; coords_e7.len()];
    match pins {
        PinSource::Injected => {
            assert_eq!(
                coords_e7.len(),
                plan.node_refs.len(),
                "injected shared-node pins coordinates/ref count mismatch for way {}: {} coords, {} refs",
                way.id(),
                coords_e7.len(),
                plan.node_refs.len(),
            );
            if let Some(bitmap) = way.shared_node_pins() {
                fill_mask_from_pin_bitmap(bitmap, &mut preserve_vertex_mask);
            }
        }
        PinSource::BlockLocal if !plan.preserve_node_refs.is_empty() => {
            const PRESERVE_LINEAR_SCAN_MAX: usize = 8;
            if plan.preserve_node_refs.len() <= PRESERVE_LINEAR_SCAN_MAX {
                for (i, node_id) in resolved_node_refs.iter().enumerate() {
                    if plan.preserve_node_refs.contains(node_id) {
                        preserve_vertex_mask[i] = true;
                    }
                }
            } else {
                let preserve_nodes: FxHashSet<i64> =
                    plan.preserve_node_refs.iter().copied().collect();
                for (i, node_id) in resolved_node_refs.iter().enumerate() {
                    if preserve_nodes.contains(node_id) {
                        preserve_vertex_mask[i] = true;
                    }
                }
            }
        }
        PinSource::BlockLocal => {}
    }
    #[allow(clippy::cast_possible_truncation)]
    let pins_marked = preserve_vertex_mask
        .iter()
        .filter(|&&pinned| pinned)
        .count() as u64;

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
    pins_marked
}

#[cfg(test)]
mod shared_node_helper_tests {
    use super::{
        detect_injected_features, fill_mask_from_pin_bitmap, member_bit, validate_and_take_members,
    };

    #[test]
    fn detect_injected_features_requires_locations() {
        for (members, pins) in [(true, false), (false, true), (true, true)] {
            assert!(detect_injected_features(members, pins, false).is_err());
        }
        let both = detect_injected_features(true, true, true).expect("locations permits flags");
        assert!(both.members);
        assert!(both.pins);
        let neither = detect_injected_features(false, false, false).expect("no flags permitted");
        assert!(!neither.members);
        assert!(!neither.pins);
    }

    #[test]
    fn way_members_validation_rejects_missing_and_mismatched() {
        assert!(validate_and_take_members(None, 0).is_err());
        assert!(validate_and_take_members(Some((&[0], 9)), 9).is_err());
        assert!(validate_and_take_members(Some((&[0, 0], 9)), 10).is_err());
        assert_eq!(
            validate_and_take_members(Some((&[0x05], 3)), 3)
                .expect("valid bitmap")
                .as_ref(),
            &[0x05]
        );
    }

    #[test]
    fn member_bit_is_lsb_first() {
        let bitmap = [0b0000_0101, 0b0000_0001];
        assert!(member_bit(&bitmap, 0));
        assert!(!member_bit(&bitmap, 1));
        assert!(member_bit(&bitmap, 2));
        assert!(!member_bit(&bitmap, 3));
        assert!(member_bit(&bitmap, 8));
    }

    #[test]
    fn pin_bitmap_mask_is_lsb_first_across_bytes() {
        let mut mask = vec![false; 10];
        fill_mask_from_pin_bitmap(&[0b0000_0101, 0b0000_0010], &mut mask);
        assert_eq!(
            mask,
            [
                true, false, true, false, false, false, false, false, false, true
            ]
        );

        let mut zeroes = vec![true; 5];
        fill_mask_from_pin_bitmap(&[0], &mut zeroes);
        assert_eq!(zeroes, [false; 5]);
    }
}
