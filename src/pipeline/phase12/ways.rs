//! Way dispatch: a dispatcher thread admits whole way blocks into rayon
//! under a count ceiling and a raw-byte budget, each task builds its block's
//! plans and processes its ways into a task-local accumulator, and a drain
//! thread owns the way index and sort writer for the duration.

use rustc_hash::FxHashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::debug::{BUSY, WAIT, wait_span};
use crate::geometry::{self, projection::Point};
use crate::node_index::{LatLonE7, NodeId, NodeStoreReader};
use crate::resources::WayAdmission;
use crate::shortbread::{self, OsmGeomType, Tags};
use crate::sort::{SortRecord, SortWriter, SpillCoalescer};
use crate::tiling::ZoomRange;
use crate::way_index::{WayId, WayIndexWriter};
use pbfhogg::{Element, Way};

use super::super::boundary_prescan::BoundaryWayMeta;
use super::super::emit::{
    EmitCtx, FeatureGeometry, LineEmitScratch, PointEmitScratch, PolygonEmitScratch, RecordSink,
    emit_match, enrich_polygon_matches, unwrap_antimeridian_path,
};
use super::super::stats::{FanoutStats, MissingRefStatsAtomic, record_fanout_from_payload_records};
use super::extent::CoordinateExtent;
use super::input::WayBlock;
use super::plans::{
    MembersForBlock, PinSource, WayPlan, build_way_plans, estimate_way_plans_bytes,
    fill_mask_from_pin_bitmap,
};

/// Per-task accumulator flush threshold. Small, because a flush is a memcpy
/// into the spill coalescer, not a chunk-file write.
const WAY_ACC_FLUSH_BYTES: usize = 8 * 1024 * 1024;

/// Counters the way phase updates from many tasks and phase12 reads once.
#[derive(Default)]
pub(super) struct WayCounters {
    /// High-water mark of admitted raw bytes (block plus plan bytes).
    pub(super) inflight_hwm: AtomicUsize,
    pub(super) ways: AtomicU64,
    pub(super) members_marked: AtomicU64,
    pub(super) pins_marked: AtomicU64,
}

/// Everything a way task reads, shared by reference across the pool.
pub(super) struct WayDispatchConfig {
    pub(super) node_reader: Option<Arc<NodeStoreReader>>,
    /// The relation plan's member-way set, or `None` when membership is
    /// injected and every way block carries its own bitmap (the set is then
    /// dropped at the first way block rather than held for the phase).
    pub(super) needed_ways: Option<FxHashSet<i64>>,
    pub(super) boundary_way_meta: BoundaryWayMeta,
    pub(super) pin_source: PinSource,
    pub(super) locations_on_ways: bool,
    pub(super) admission: WayAdmission,
    pub(super) window: ZoomRange,
    pub(super) fanout_caps: [Option<std::num::NonZeroU32>; shortbread::Layer::count()],
    pub(super) polygon_simplify_factor: f64,
    pub(super) sort_chunk_budget: usize,
    pub(super) spill: Arc<SpillCoalescer>,
    pub(super) missing_ref_stats: Arc<MissingRefStatsAtomic>,
    pub(super) counters: Arc<WayCounters>,
}

/// What the drain hands back once the last task result is written.
pub(super) struct WayDrained {
    pub(super) way_index: WayIndexWriter,
    pub(super) sort_writer: SortWriter,
    pub(super) features: u64,
    pub(super) fanout: FanoutStats,
    pub(super) extent: CoordinateExtent,
}

/// The running dispatcher and drain. Dropping it without
/// [`WayDispatch::finish`] (an error return) still closes the block channel
/// and joins both threads, so a failed run has stopped writing chunks and way
/// index entries before it returns.
pub(super) struct WayDispatch {
    block_tx: Option<std::sync::mpsc::SyncSender<WayBlock>>,
    dispatcher: Option<std::thread::JoinHandle<()>>,
    drain: Option<std::thread::JoinHandle<WayDrained>>,
}

impl WayDispatch {
    pub(super) fn spawn(
        cfg: WayDispatchConfig,
        way_index: WayIndexWriter,
        sort_writer: SortWriter,
    ) -> Self {
        let (block_tx, block_rx) = std::sync::mpsc::sync_channel::<WayBlock>(1);
        // Capacity must be at least the in-flight task ceiling: rayon tasks
        // block on send() while holding a rayon thread. If capacity < inflight
        // tasks, blocked senders tie up all rayon threads, the dispatcher
        // (which runs inside rayon::in_place_scope) can't make progress, and
        // the pool deadlocks.
        let (result_tx, result_rx) =
            std::sync::mpsc::sync_channel::<WayTaskResult>(cfg.admission.max_inflight);

        let dispatcher = std::thread::spawn(move || dispatch(&cfg, &block_rx, result_tx));

        let drain = std::thread::spawn(move || {
            let mut drained = WayDrained {
                way_index,
                sort_writer,
                features: 0,
                fanout: FanoutStats::new(),
                extent: CoordinateExtent::default(),
            };
            while let Ok(result) = result_rx.recv() {
                let _busy = wait_span(&BUSY.phase12_drain);
                drained.features += drain_way_task_result(
                    result,
                    &mut drained.way_index,
                    &mut drained.sort_writer,
                    &mut drained.fanout,
                    &mut drained.extent,
                );
            }
            drained
        });

        Self {
            block_tx: Some(block_tx),
            dispatcher: Some(dispatcher),
            drain: Some(drain),
        }
    }

    /// Forward one way block; blocks while the dispatcher waits for
    /// admission (backpressure on the ordered consumer).
    pub(super) fn send(&self, block: WayBlock) {
        let _wait = wait_span(&WAIT.way_block_send);
        self.block_tx
            .as_ref()
            .expect("way dispatch already finished")
            .send(block)
            .expect("way dispatcher thread panicked");
    }

    /// Close the block channel, let every admitted task finish, then take
    /// back the drain's resources. The dispatcher owns the only result sender
    /// outside the tasks, so its exit is what ends the drain.
    pub(super) fn finish(mut self) -> WayDrained {
        drop(self.block_tx.take());
        self.dispatcher
            .take()
            .expect("way dispatcher already joined")
            .join()
            .expect("way dispatcher thread panicked");
        self.drain
            .take()
            .expect("way drain already joined")
            .join()
            .expect("way drain thread panicked")
    }
}

impl Drop for WayDispatch {
    fn drop(&mut self) {
        drop(self.block_tx.take());
        if let Some(handle) = self.dispatcher.take() {
            drop(handle.join());
        }
        if let Some(handle) = self.drain.take() {
            drop(handle.join());
        }
    }
}

/// The way-phase admission control: a count ceiling (primary) and a raw-byte
/// budget (safety net against individually huge blocks; see the default's
/// comment in `crate::resources`). A claim waits and returns a ticket; plan
/// bytes are added to the ticket without waiting; dropping the ticket - after
/// the task's result is sent, or on unwind if the task panics - releases
/// everything it holds, so a panicking task cannot leave the dispatcher
/// asleep on capacity that will never come back.
struct Admission<'a> {
    inflight: std::sync::Mutex<(usize, usize)>,
    cvar: std::sync::Condvar,
    max_inflight: usize,
    budget: usize,
    hwm: &'a AtomicUsize,
}

impl<'a> Admission<'a> {
    fn new(admission: WayAdmission, hwm: &'a AtomicUsize) -> Self {
        Self {
            inflight: std::sync::Mutex::new((0, 0)),
            cvar: std::sync::Condvar::new(),
            max_inflight: admission.max_inflight,
            budget: admission.budget,
            hwm,
        }
    }

    /// Wait for capacity for a block of `cost` bytes. Always admits at least
    /// one task - a single block over the byte budget must not deadlock the
    /// condvar (no in-flight tasks means no notify, a permanent sleep).
    fn lock(&self) -> std::sync::MutexGuard<'_, (usize, usize)> {
        // No code panics while holding this lock, so poisoning can only come
        // from an unrelated unwind; the counts stay consistent either way.
        self.inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn claim(&self, cost: usize) -> Ticket<'_> {
        let _wait = wait_span(&WAIT.way_budget);
        let mut guard = self
            .cvar
            .wait_while(self.lock(), |&mut (count, bytes)| {
                count >= self.max_inflight || (count > 0 && bytes + cost > self.budget)
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.0 += 1;
        guard.1 += cost;
        self.hwm.fetch_max(guard.1, Ordering::Relaxed);
        Ticket {
            admission: self,
            bytes: cost,
        }
    }
}

/// One admitted task's share of the in-flight count and bytes.
struct Ticket<'a> {
    admission: &'a Admission<'a>,
    bytes: usize,
}

impl Ticket<'_> {
    /// Account bytes learned inside a task without waiting: blocking a rayon
    /// task on the budget condvar can deadlock the pool (every thread
    /// waiting, no completions to free budget).
    fn add(&mut self, bytes: usize) {
        let mut guard = self.admission.lock();
        guard.1 += bytes;
        self.bytes += bytes;
        self.admission.hwm.fetch_max(guard.1, Ordering::Relaxed);
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        let mut guard = self.admission.lock();
        guard.0 -= 1;
        guard.1 -= self.bytes;
        self.admission.cvar.notify_one();
    }
}

/// The dispatcher loop. Plan build happens inside the spawned task, not
/// here: this loop is the pipeline stage the ordered consumer blocks behind
/// (way_block_send), so any serial work here rate-limits the whole PBF read.
/// Measured on germany locations: 26.9s of serial build_way_plans - half of
/// phase12. The admission claim therefore uses the block's decompressed size
/// alone; the task adds the plans' measured bytes once built (bounded
/// overshoot: at most max_inflight blocks' plan bytes escape the wait).
fn dispatch(
    cfg: &WayDispatchConfig,
    block_rx: &std::sync::mpsc::Receiver<WayBlock>,
    result_tx: std::sync::mpsc::SyncSender<WayTaskResult>,
) {
    let admission = Admission::new(cfg.admission, &cfg.counters.inflight_hwm);
    let admission = &admission;
    let acc_flush_bytes = cfg.sort_chunk_budget.min(WAY_ACC_FLUSH_BYTES);
    rayon::in_place_scope(|s| {
        while let Ok(way_block) = block_rx.recv() {
            let block_cost = way_block.block.decompressed_size();
            let mut ticket = admission.claim(block_cost);
            let tx = result_tx.clone();
            s.spawn(move |_| {
                run_way_task(cfg, &way_block, acc_flush_bytes, &mut ticket, &tx);
                // The ticket drops here, after the result send.
            });
        }
    });
    // Dropped after every task's clone: the drain's recv() returns Err and
    // it exits.
    drop(result_tx);
}

/// One block's task: build its plans, process its ways, ship the residual.
/// The plans' bytes are added to the task's admission ticket.
fn run_way_task(
    cfg: &WayDispatchConfig,
    way_block: &WayBlock,
    acc_flush_bytes: usize,
    ticket: &mut Ticket<'_>,
    tx: &std::sync::mpsc::SyncSender<WayTaskResult>,
) {
    // Summed across rayon workers, not a serial stage: read
    // phase12_plan_build_ns as thread-time.
    let plan_busy = wait_span(&BUSY.phase12_plan_build);
    let members = match &cfg.needed_ways {
        None => MembersForBlock::Bitmap(
            way_block
                .members
                .as_deref()
                .expect("way block without members bitmap on injected path"),
        ),
        Some(needed_ways) => MembersForBlock::Set(needed_ways),
    };
    let (plans, marked, location_extent) = build_way_plans(
        &way_block.block,
        &members,
        cfg.pin_source,
        cfg.locations_on_ways,
    );
    drop(plan_busy);
    cfg.counters
        .ways
        .fetch_add(plans.len() as u64, Ordering::Relaxed);
    cfg.counters
        .members_marked
        .fetch_add(marked, Ordering::Relaxed);
    ticket.add(estimate_way_plans_bytes(&plans));

    let ctx = WayCtx {
        node_reader: cfg.node_reader.as_deref(),
        window: cfg.window,
        missing_ref_stats: &cfg.missing_ref_stats,
        fanout_caps: &cfg.fanout_caps,
        polygon_simplify_factor: cfg.polygon_simplify_factor,
        pins: cfg.pin_source,
        boundary_way_meta: &cfg.boundary_way_meta,
    };
    let mut acc = WayAcc::new();
    acc.location_extent = location_extent;
    let mut plans = plans.into_iter();
    for element in way_block.block.elements() {
        let Element::Way(way) = element else {
            continue;
        };
        let Some(plan) = plans.next() else {
            continue;
        };
        debug_assert_eq!(
            plan.way_id.as_i64(),
            way.id(),
            "way plan misaligned with block ways"
        );
        let pins_marked = process_planned_way_into(&way, &plan, &ctx, &mut acc);
        cfg.counters
            .pins_marked
            .fetch_add(pins_marked, Ordering::Relaxed);
        if acc.bytes >= acc_flush_bytes {
            acc.flush(&cfg.spill);
        }
    }
    // The acc dies with the task: residual records flush to the shared
    // coalescer, way_index puts and the tally ship through the drain. Accs
    // deliberately do NOT outlive their task - a phase-lifetime acc pool
    // ratcheted each acc's scratch capacity (sink burst, pyramid, overlay
    // engine buffers) to the worst feature it ever saw: 24 accs x ~770 MB =
    // 18.5 GB of phase12 anon at NA scale, page-cache eviction, and a 2M
    // major-fault storm in the relation tail. Scratch reuse across the ~4000
    // ways WITHIN a block is where the de-churn win lives; reuse across
    // blocks bought nothing measurable.
    //
    // The bulk of the record volume drains through the shared spill
    // coalescer instead of funneling through the drain thread's serial
    // sort_writer pushes - measured on germany locations: 12.3 GB of records
    // through one thread, 141s of tasks blocked on way_result_send behind it.
    acc.flush(&cfg.spill);
    {
        // Blocked here means the drain thread is the choke - tasks queue
        // behind its result channel while holding a rayon thread.
        let _wait = wait_span(&WAIT.way_result_send);
        // A closed result channel means the drain is gone, which only
        // happens when its thread panicked; that panic surfaces at its join.
        drop(tx.send(acc.finish()));
    }
}

pub(super) struct WayTaskResult {
    pub(super) count: u64,
    pub(super) sink: RecordSink,
    pub(super) fanout: FanoutStats,
    pub(super) way_puts: Vec<(WayId, Vec<LatLonE7>)>,
    pub(super) location_extent: CoordinateExtent,
}

/// A task-local accumulator: the record sink plus scratch reused across the
/// ways of one block.
pub(super) struct WayAcc {
    pub(super) sink: RecordSink,
    pub(super) bytes: usize,
    /// Records already handed to the spill coalescer by `flush`; the drain
    /// adds the residual sink's records when it writes them.
    pub(super) count: u64,
    pub(super) fanout: FanoutStats,
    pub(super) way_puts: Vec<(WayId, Vec<LatLonE7>)>,
    pub(super) location_extent: CoordinateExtent,
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
            location_extent: CoordinateExtent::default(),
            merc: Vec::new(),
            point_emit: PointEmitScratch::new(),
            line_emit: LineEmitScratch::new(),
            polygon_emit: PolygonEmitScratch::new(),
        }
    }

    /// Hand the sink's records to the shared spill coalescer (a memcpy under
    /// its lock) and reset for reuse. Tally stays in the sink and is merged
    /// when the residual acc ships through the drain at end of stream.
    pub(super) fn flush(&mut self, spill: &SpillCoalescer) {
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
            location_extent: self.location_extent,
        }
    }
}

#[hotpath::measure]
pub(super) fn drain_way_task_result(
    result: WayTaskResult,
    way_index: &mut WayIndexWriter,
    sort_writer: &mut SortWriter,
    fanout: &mut FanoutStats,
    location_extent: &mut CoordinateExtent,
) -> u64 {
    location_extent.merge(result.location_extent);
    for (way_id, coords_e7) in result.way_puts {
        way_index.put(way_id, &coords_e7);
    }
    sort_writer.merge_tally(&result.sink.tally);
    fanout.merge(&result.fanout);
    // Flushed records plus the residual written here: each record is counted
    // exactly once, on whichever side of the flush it left the acc.
    let count = result.count + result.sink.records.len() as u64;
    for record in result.sink.records {
        sort_writer
            .push_untracked(SortRecord {
                key: record.key,
                data: record.bytes(&result.sink.payload).into(),
            })
            .expect("sort push failed");
    }
    count
}

/// The per-run inputs `process_planned_way_into` reads for every way.
pub(super) struct WayCtx<'a> {
    pub(super) node_reader: Option<&'a NodeStoreReader>,
    pub(super) window: ZoomRange,
    pub(super) missing_ref_stats: &'a MissingRefStatsAtomic,
    pub(super) fanout_caps: &'a [Option<std::num::NonZeroU32>],
    pub(super) polygon_simplify_factor: f64,
    pub(super) pins: PinSource,
    pub(super) boundary_way_meta: &'a BoundaryWayMeta,
}

/// Process one planned way into a task-local accumulator. Returns the number
/// of vertices pinned for DP.
#[hotpath::measure]
// cognitive_complexity is visible only without the hotpath feature, whose
// measure macro re-wraps the body.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub(super) fn process_planned_way_into(
    way: &Way<'_>,
    plan: &WayPlan,
    ctx: &WayCtx<'_>,
    acc: &mut WayAcc,
) -> u64 {
    let way_id = plan.way_id.as_i64();
    let boundary_way_meta = ctx.boundary_way_meta;
    let tags_ref: Vec<(&str, &str)> = way.tags().collect();
    if tags_ref.is_empty() && !plan.is_member && !boundary_way_meta.contains_key(&way_id) {
        return 0;
    }
    let tag_helper = Tags(&tags_ref);
    // Members are always resolved (a relation reads their geometry back), so the
    // both-geom pre-filter is only worth computing for non-members, where it
    // gates the early return. Computing it for members would run two
    // `match_element` passes whose result is never inspected.
    if !plan.is_member && !boundary_way_meta.contains_key(&way_id) {
        let possible_feature = !shortbread::match_element(&tag_helper, OsmGeomType::ClosedWay)
            .is_empty()
            || !shortbread::match_element(&tag_helper, OsmGeomType::OpenWay).is_empty();
        if !possible_feature {
            return 0;
        }
    }

    let (coords_e7, resolved_node_refs): (Vec<LatLonE7>, Vec<i64>) =
        if let Some(nr) = ctx.node_reader {
            let mut missing_refs: usize = 0;
            let mut resolved: Vec<LatLonE7> = Vec::with_capacity(plan.node_refs.len());
            let mut resolved_refs: Vec<i64> = Vec::with_capacity(plan.node_refs.len());
            for &id in &plan.node_refs {
                if let Some(coord) = nr.get_coord_id(NodeId::new(id)) {
                    resolved.push(coord);
                    resolved_refs.push(id);
                } else {
                    missing_refs += 1;
                }
            }
            if missing_refs > 0 {
                ctx.missing_ref_stats.record_way_missing_nodes(missing_refs);
            }
            (resolved, resolved_refs)
        } else {
            let coords: Vec<LatLonE7> = way
                .node_locations()
                .map(|loc| LatLonE7::new(loc.decimicro_lat(), loc.decimicro_lon()))
                .collect();
            (coords, plan.node_refs.clone())
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
    resolve_boundary_match(&tag_helper, way_id, boundary_way_meta, &mut matches);

    if matches.is_empty() {
        return 0;
    }

    #[allow(clippy::cast_sign_loss)]
    let osm_id = way_id as u64;
    let before = acc.sink.records.len();
    let mut preserve_vertex_mask: Vec<bool> = vec![false; coords_e7.len()];
    match ctx.pins {
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
            .map(|coord| geometry::projection::project_e7(coord.lat, coord.lon)),
    );
    let _ = unwrap_antimeridian_path(&mut acc.merc, is_closed);

    {
        let merc = acc.merc.as_slice();

        if is_closed {
            let area_m2 = geometry::polygon::area_sq_meters(merc);
            enrich_polygon_matches(&mut matches, area_m2);
        }

        let emit_ctx = EmitCtx {
            window: ctx.window,
            fanout_caps: ctx.fanout_caps,
            polygon_simplify_factor: ctx.polygon_simplify_factor,
        };
        for m in &matches {
            emit_match(
                osm_id,
                FeatureGeometry::Way {
                    coords: merc,
                    pins: &preserve_vertex_mask,
                    line: &mut acc.line_emit,
                    polygon: &mut acc.polygon_emit,
                    fanout: &mut acc.fanout,
                },
                m,
                &emit_ctx,
                &mut acc.sink,
                &mut acc.point_emit,
            );
        }
    }
    record_fanout_from_payload_records(&acc.sink.records[before..], &mut acc.fanout);
    acc.bytes = acc.sink.bytes();
    pins_marked
}

fn resolve_boundary_match(
    tags: &Tags<'_>,
    way_id: i64,
    boundary_way_meta: &BoundaryWayMeta,
    matches: &mut smallvec::SmallVec<[shortbread::LayerMatch; 4]>,
) {
    let Some(resolved) =
        shortbread::boundaries::boundary_line(tags, boundary_way_meta.get(&way_id).copied())
    else {
        return;
    };
    if let Some(existing) = matches
        .iter_mut()
        .find(|m| m.layer == shortbread::Layer::Boundaries)
    {
        *existing = resolved;
    } else {
        matches.push(resolved);
    }
}

#[cfg(test)]
mod boundary_synthesis_tests {
    use super::super::super::boundary_prescan::{BoundaryWayMeta, fold_relation};
    use super::resolve_boundary_match;
    use crate::shortbread::{self, AttrValue, Layer, Tags};
    use smallvec::SmallVec;

    fn synthesize(
        way_tags: &[(&str, &str)],
        meta: &BoundaryWayMeta,
    ) -> SmallVec<[shortbread::LayerMatch; 4]> {
        let mut matches: SmallVec<[shortbread::LayerMatch; 4]> = SmallVec::new();
        resolve_boundary_match(&Tags(way_tags), 42, meta, &mut matches);
        matches
    }

    fn admin_level(m: &shortbread::LayerMatch) -> i64 {
        for (k, v, _) in &m.attrs {
            if *k == "admin_level"
                && let AttrValue::Int(n) = v
            {
                return *n;
            }
        }
        panic!("expected an admin_level int attribute");
    }

    fn bool_attr(m: &shortbread::LayerMatch, key: &str) -> bool {
        for (k, v, _) in &m.attrs {
            if *k == key
                && let AttrValue::Bool(b) = v
            {
                return *b;
            }
        }
        panic!("expected a {key} bool attribute");
    }

    #[test]
    fn shared_member_takes_min_admin_level() {
        let mut meta = BoundaryWayMeta::default();
        fold_relation(
            &Tags(&[("boundary", "administrative"), ("admin_level", "2")]),
            [42],
            &mut meta,
        );
        fold_relation(
            &Tags(&[("boundary", "administrative"), ("admin_level", "4")]),
            [42],
            &mut meta,
        );
        let matches = synthesize(&[], &meta);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].layer, Layer::Boundaries);
        assert_eq!(admin_level(&matches[0]), 2, "min of the two relations");
        assert_eq!(matches[0].min_zoom, 0);
    }

    #[test]
    fn untagged_member_synthesizes_match() {
        let mut meta = BoundaryWayMeta::default();
        fold_relation(
            &Tags(&[("boundary", "administrative"), ("admin_level", "2")]),
            [42],
            &mut meta,
        );
        // Way carries no admin tags of its own: the member must still emit.
        let matches = synthesize(&[], &meta);
        assert_eq!(matches.len(), 1);
        assert_eq!(admin_level(&matches[0]), 2);
    }

    #[test]
    fn maritime_comes_from_way_own_tags() {
        let mut meta = BoundaryWayMeta::default();
        fold_relation(
            &Tags(&[("boundary", "administrative"), ("admin_level", "2")]),
            [42],
            &mut meta,
        );
        let matches = synthesize(&[("natural", "coastline")], &meta);
        assert_eq!(matches.len(), 1);
        assert!(
            bool_attr(&matches[0], "maritime"),
            "maritime from way-own natural=coastline"
        );
        assert_eq!(admin_level(&matches[0]), 2);
    }

    #[test]
    fn independent_admin_and_disputed_parentage() {
        let mut meta = BoundaryWayMeta::default();
        fold_relation(
            &Tags(&[("boundary", "administrative"), ("admin_level", "2")]),
            [42],
            &mut meta,
        );
        fold_relation(&Tags(&[("boundary", "disputed")]), [42], &mut meta);
        let matches = synthesize(&[], &meta);
        assert_eq!(matches.len(), 1);
        assert_eq!(admin_level(&matches[0]), 2);
        assert!(
            bool_attr(&matches[0], "disputed"),
            "disputed OR'd from the disputed parent"
        );
    }

    #[test]
    fn no_meta_and_no_own_tags_synthesizes_nothing() {
        let meta = BoundaryWayMeta::default();
        assert!(synthesize(&[("highway", "residential")], &meta).is_empty());
    }
}
