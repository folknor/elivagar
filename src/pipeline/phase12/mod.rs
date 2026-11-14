//! Phase 1+2: one PBF read that emits every node and way feature into the
//! sort, then the relation tail.
//!
//! Each concurrent component owns its own lifecycle - start, feed, finish,
//! and quiescence on an error return - in its own module: the node workers
//! (`nodes`), the way dispatcher and drain with their admission control
//! (`ways`), the relation prepass (`prepass`) and the unordered locations
//! source (`input`). The [`Router`] here owns only the ordering between them.

mod extent;
mod input;
mod nodes;
mod plans;
mod prepass;
mod ways;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::debug::{BUSY, WAIT, wait_span};
use crate::geometry::projection::MercBbox;
use crate::node_index::NodeStore;
use crate::resources::{Resources, WayAdmission};
use crate::sort::{SortWriter, SpillCoalescer};
use crate::way_index::WayIndexWriter;
use pbfhogg::{BlobFilter, BlockType, ElementReader, PrimitiveBlock};

use super::relations::process_relation_blocks;
use super::stats::{FanoutStats, MissingRefStatsAtomic, Phase12Stats};
use super::{PipelineError, RunPlan, SORT_CHUNKS_DIR};
use crate::tiling::ZoomRange;
use extent::CoordinateExtent;
use input::{UnorderedBlockSource, WayBlock};
use nodes::{NodeOutcome, NodeWorkers};
use plans::MemberSource;
pub(super) use plans::PinSource;
use prepass::Prepass;
use ways::{WayCounters, WayDispatch, WayDispatchConfig};

pub(super) use input::resolve_locations_on_ways;

#[cfg(test)]
pub(super) use extent::{crosses_antimeridian, lon_e7_shifted_360};
#[cfg(test)]
pub(super) use input::{NodeStoreMode, select_node_store_mode};
#[cfg(test)]
pub(super) use plans::{MembersForBlock, RawWay, annotate_block_shared_node_refs, build_way_plans};

/// Node routing. The raw path's single store-owning worker ends at the first
/// way block, which takes the store back to build its reader; a node block
/// after that would silently lose puts, so the input is refused - block
/// order is the file's, and the raw path needs every node block before the
/// first way block. Relation blocks do not end the node phase. Locations
/// mode runs a storeless pool for the whole read.
enum NodeRoute {
    Stored(StoredNodes),
    Inline {
        workers: Option<NodeWorkers>,
        count: usize,
    },
}

enum StoredNodes {
    /// No node block yet. Still the state at the first way block when the
    /// input has none, which ends the node phase all the same.
    Pending(Box<NodeStore>),
    Running(NodeWorkers),
    Ended,
}

/// Relation blocks are buffered until the read ends, because locations-mode
/// PBFs can carry way blocks after relation blocks and the tail needs the
/// finished way index.
///
/// The buffer is byte-capped: this stock is input-scaled (planet holds
/// several GB of decompressed relation blocks). Past the cap everything
/// buffered is dropped, buffering never resumes, and the tail re-reads
/// relation blobs from the PBF via `BlobFilter::only_relations` (indexed PBFs
/// skip-read).
struct RelationBuffer {
    cap: usize,
    blocks: Vec<PrimitiveBlock>,
    /// Cumulative decompressed bytes of every relation block seen, including
    /// those seen after the cap tripped.
    bytes: usize,
    spilled: bool,
}

impl RelationBuffer {
    fn push(&mut self, block: PrimitiveBlock) {
        self.bytes += block.decompressed_size();
        if self.spilled {
            return;
        }
        if self.bytes > self.cap {
            self.spilled = true;
            self.blocks = Vec::new();
            eprintln!(
                "  Relation blocks exceed {} MB buffer cap - re-reading at tail",
                self.cap / (1024 * 1024)
            );
        } else {
            self.blocks.push(block);
        }
    }
}

/// Run-wide inputs the router hands to the components it starts.
struct RouteCtx<'a> {
    config: &'a RunPlan,
    window: ZoomRange,
    locations_on_ways: bool,
    member_source: MemberSource,
    pin_source: PinSource,
    admission: WayAdmission,
    sort_chunk_budget: usize,
    spill: Arc<SpillCoalescer>,
    missing_ref_stats: Arc<MissingRefStatsAtomic>,
    way_counters: Arc<WayCounters>,
}

/// Block routing during the read: classifies each block and feeds the
/// component that owns it, starting components lazily on their first block.
struct Router<'a> {
    ctx: RouteCtx<'a>,
    nodes: NodeRoute,
    node_outcome: NodeOutcome,
    node_store_stats: Option<(u64, usize)>,
    prepass: Prepass,
    /// The way index, until the way dispatch takes it.
    way_index: Option<WayIndexWriter>,
    /// Every record reaches the sort through the spill coalescer; the writer
    /// stays here and only takes the tallies.
    sort_writer: SortWriter,
    ways: Option<WayDispatch>,
    relation_plan_needed_ways: usize,
    relation_plan_superset_ways: usize,
    relations: RelationBuffer,
}

/// What the read hands to the relation tail and the stats.
struct Routed {
    way_index: WayIndexWriter,
    sort_writer: SortWriter,
    node_count: u64,
    features_emitted: u64,
    extent: CoordinateExtent,
    fanout: FanoutStats,
    node_store_stats: Option<(u64, usize)>,
    relation_plan_needed_ways: usize,
    relation_plan_superset_ways: usize,
    relations: RelationBuffer,
}

impl Router<'_> {
    fn route(
        &mut self,
        block: PrimitiveBlock,
        members: Option<Box<[u8]>>,
    ) -> Result<(), PipelineError> {
        // Classify block by reading first wire tag byte per group - no
        // element decoding. Sorted PBFs have single-type blocks.
        match block.block_type() {
            BlockType::DenseNodes | BlockType::Nodes => self.route_nodes(block)?,
            BlockType::Ways => {
                if self.ways.is_none() {
                    self.start_way_phase()?;
                }
                self.ways
                    .as_ref()
                    .expect("way dispatch started above")
                    .send(WayBlock { block, members });
            }
            BlockType::Relations => self.relations.push(block),
            BlockType::Empty | BlockType::Mixed => {}
        }
        Ok(())
    }

    fn route_nodes(&mut self, block: PrimitiveBlock) -> Result<(), PipelineError> {
        let ctx = &self.ctx;
        match &mut self.nodes {
            NodeRoute::Stored(state) => {
                *state = match std::mem::replace(state, StoredNodes::Ended) {
                    StoredNodes::Pending(store) => {
                        StoredNodes::Running(NodeWorkers::stored(*store, &ctx.spill, ctx.window))
                    }
                    StoredNodes::Running(workers) => StoredNodes::Running(workers),
                    StoredNodes::Ended => {
                        return Err(PipelineError::input(
                            "node block after the first way block: without locations on ways, \
                             every node must precede the ways (sort it with `pbfhogg sort`)",
                        ));
                    }
                };
                if let StoredNodes::Running(workers) = state {
                    workers.send(block);
                }
            }
            // The single worker was the node-phase rate limiter in locations
            // mode (16.6s serial on NA locations, node_block_send wait 16.5s
            // from the consumer side); tagged-node emission and extent
            // tracking are order-free there, so blocks fan out to a pool.
            NodeRoute::Inline { workers, count } => workers
                .get_or_insert_with(|| NodeWorkers::inline(*count, &ctx.spill, ctx.window))
                .send(block),
        }
        Ok(())
    }

    /// The raw path's node phase ends here: join its worker and take the
    /// store back. `None` in locations mode.
    fn end_stored_node_phase(&mut self) -> Result<Option<NodeStore>, PipelineError> {
        let NodeRoute::Stored(state) = &mut self.nodes else {
            return Ok(None);
        };
        Ok(match std::mem::replace(state, StoredNodes::Ended) {
            StoredNodes::Pending(store) => Some(*store),
            StoredNodes::Running(workers) => {
                workers.join(&mut self.node_outcome)?;
                self.node_outcome.node_store.take()
            }
            StoredNodes::Ended => None,
        })
    }

    // The expects are routing invariants (store and way index present
    // exactly once), not input errors.
    #[allow(clippy::unwrap_in_result)]
    fn start_way_phase(&mut self) -> Result<(), PipelineError> {
        let node_reader = if self.ctx.locations_on_ways {
            eprintln!("  LocationsOnWays mode - processing ways (no node store)...");
            None
        } else {
            let store = self
                .end_stored_node_phase()?
                .expect("raw path reached the way phase without its node store");
            let reader = Arc::new(store.into_reader().map_err(|err| {
                PipelineError::io("failed to convert the node store to a reader", err)
            })?);
            let store_stats = reader.stats();
            self.node_store_stats = store_stats
                .group_count
                .map(|group_count| (store_stats.node_count, group_count));
            eprintln!(
                "  Node store finalized ({} nodes), processing ways...",
                self.node_outcome.node_count
            );
            Some(reader)
        };

        // The relation plan is first needed here. Its join blocks the
        // ordered consumer, so it is stall time.
        let relation_plan = {
            let _wait = wait_span(&WAIT.prepass_join);
            self.prepass.join()?
        };
        let needed_ways = match self.ctx.member_source {
            MemberSource::Plan => {
                self.relation_plan_needed_ways = relation_plan.needed_ways.len();
                self.relation_plan_superset_ways = relation_plan.superset_ways_count;
                Some(relation_plan.needed_ways)
            }
            MemberSource::Injected => None,
        };

        let ctx = &self.ctx;
        let cfg = WayDispatchConfig {
            node_reader,
            needed_ways,
            boundary_way_meta: relation_plan.boundary_way_meta,
            pin_source: ctx.pin_source,
            locations_on_ways: ctx.locations_on_ways,
            admission: ctx.admission,
            window: ctx.window,
            fanout_caps: *ctx.config.contract.producer.fanout_caps(),
            polygon_simplify_factor: ctx.config.contract.producer.simplify_factor(),
            sort_chunk_budget: ctx.sort_chunk_budget,
            spill: Arc::clone(&ctx.spill),
            missing_ref_stats: Arc::clone(&ctx.missing_ref_stats),
            counters: Arc::clone(&ctx.way_counters),
        };
        let way_index = self
            .way_index
            .take()
            .expect("way index taken before the way phase");
        self.ways = Some(WayDispatch::spawn(cfg, way_index));
        Ok(())
    }

    /// End of read: join the node workers (the raw path's already ended at
    /// its first way block unless the input had none), release the node
    /// store, join a prepass no way block joined, finish the way dispatch,
    /// and apply the node and way tallies to the sort writer.
    fn finish(self) -> Result<Routed, PipelineError> {
        let Self {
            ctx,
            nodes,
            mut node_outcome,
            node_store_stats,
            prepass,
            way_index,
            mut sort_writer,
            ways,
            relation_plan_needed_ways,
            relation_plan_superset_ways,
            relations,
        } = self;
        match nodes {
            NodeRoute::Stored(StoredNodes::Running(workers))
            | NodeRoute::Inline {
                workers: Some(workers),
                ..
            } => workers.join(&mut node_outcome)?,
            NodeRoute::Stored(StoredNodes::Pending(_) | StoredNodes::Ended)
            | NodeRoute::Inline { workers: None, .. } => {}
        }
        // Nothing after the way phase reads the node store - release it.
        drop(node_outcome.node_store.take());

        prepass.finish()?;

        let (way_index, way_features, fanout, way_extent) = match ways {
            Some(dispatch) => {
                let drained = dispatch.finish();
                // A failed way task stopped its block, and a failed way index
                // write poisoned the writer; the drained state is incomplete,
                // so it is dropped with the failure before anything reads it.
                if let Some(err) = ctx.way_counters.take_failure() {
                    return Err(err);
                }
                sort_writer.merge_tally(&drained.tally);
                (
                    drained.way_index,
                    drained.features,
                    drained.fanout,
                    drained.extent,
                )
            }
            None => (
                way_index.expect("way index neither dispatched nor held"),
                0,
                FanoutStats::new(),
                CoordinateExtent::default(),
            ),
        };
        if let Some(tally) = node_outcome.tally.take() {
            sort_writer.merge_tally(&tally);
        }
        let mut extent = node_outcome.extent;
        extent.merge(way_extent);
        Ok(Routed {
            way_index,
            sort_writer,
            node_count: node_outcome.node_count,
            features_emitted: node_outcome.features_emitted + way_features,
            extent,
            fanout,
            node_store_stats,
            relation_plan_needed_ways,
            relation_plan_superset_ways,
            relations,
        })
    }
}

#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub(super) fn phase_read_and_process(
    config: &RunPlan,
    resources: &Resources,
) -> Result<(SortWriter, MercBbox, Phase12Stats), PipelineError> {
    let window = config.window;
    eprintln!("\n--- Phase 1+2: Reading PBF + processing features ---");

    let sort_chunk_budget = resources.sort_chunk;
    let sort_writer = SortWriter::new(
        &config.tmp_dir.join(SORT_CHUNKS_DIR),
        sort_chunk_budget,
        config.compress_sort_chunks,
    )?;

    // Worker sizing (crate::resources), not a process-wide cap; Rayon is
    // sized separately by the caller. Relation prepass, way dispatch and
    // result drain each have a separate OS thread.
    let decode_threads = resources.decode_threads;
    let reader = ElementReader::from_path(&config.pbf_path)
        .map_err(|e| PipelineError::failed("failed to open PBF").with_cause(e))?
        .decode_threads(decode_threads);

    let header = reader.header();
    let injected = input::detect_injected_features(
        header.has_way_members_v1(),
        header.has_shared_node_pins_v1(),
        header.has_locations_on_ways(),
    )?;
    let member_source = if injected.members {
        eprintln!("  WayMembers-v1 detected - using injected membership");
        MemberSource::Injected
    } else {
        MemberSource::Plan
    };
    let pin_source = if injected.pins {
        eprintln!("  SharedNodePins-v1 detected - using injected pins");
        PinSource::Injected
    } else {
        PinSource::BlockLocal
    };
    let locations_on_ways =
        resolve_locations_on_ways(config.locations_on_ways, header.has_locations_on_ways())?;
    let way_admission = resources.way_admission(locations_on_ways);
    way_admission.emit();

    let pbf_size = std::fs::metadata(&config.pbf_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let header_sorted = header.is_sorted();
    let node_store_mode = input::select_node_store_mode(
        locations_on_ways,
        header_sorted,
        config.force_sorted,
        pbf_size,
        config.allow_unsafe_flat_index,
    )?;
    let nodes = match input::create_node_store(node_store_mode, config, header_sorted)? {
        Some(store) => NodeRoute::Stored(StoredNodes::Pending(Box::new(store))),
        None => NodeRoute::Inline {
            workers: None,
            count: way_admission.node_workers,
        },
    };
    let way_index = WayIndexWriter::create(&config.tmp_dir)?;

    // The sort writer owns chunk allocation. Its cloneable sink is shared by
    // the drain writer and spill coalescer for concurrent, collision-free
    // writes; the coalescer outlives node workers, way tasks and the
    // relation tail, so it is finished only after all three.
    let spill = Arc::new(SpillCoalescer::new(
        sort_writer.chunk_sink(),
        sort_chunk_budget,
    ));
    let missing_ref_stats = Arc::new(MissingRefStatsAtomic::default());
    let way_counters = Arc::new(WayCounters::default());

    let mut router = Router {
        ctx: RouteCtx {
            config,
            window,
            locations_on_ways,
            member_source,
            pin_source,
            admission: way_admission,
            sort_chunk_budget,
            spill: Arc::clone(&spill),
            missing_ref_stats: Arc::clone(&missing_ref_stats),
            way_counters: Arc::clone(&way_counters),
        },
        nodes,
        node_outcome: NodeOutcome::default(),
        node_store_stats: None,
        prepass: Prepass::spawn(&config.pbf_path, decode_threads),
        way_index: Some(way_index),
        sort_writer,
        ways: None,
        relation_plan_needed_ways: 0,
        relation_plan_superset_ways: 0,
        relations: RelationBuffer {
            cap: resources.rel_blocks_buffer_cap,
            blocks: Vec::new(),
            bytes: 0,
            spilled: false,
        },
    };

    if locations_on_ways {
        // Elivagar-owned bounded read: see UnorderedBlockSource. Everything
        // downstream of this loop is order-free in locations mode.
        let source = UnorderedBlockSource::spawn(
            &config.pbf_path,
            decode_threads,
            injected.members,
            injected.pins,
        )?;
        drop(reader);
        while let Some(item) = source.recv() {
            let (block, members) = item?;
            router.route(block, members)?;
        }
        source.finish();
    } else {
        let blocks = reader
            .into_blocks_pipelined()
            .map_err(|e| PipelineError::failed("PBF pipeline start failed").with_cause(e))?;
        for block_result in blocks {
            let block =
                block_result.map_err(|e| PipelineError::failed("PBF read failed").with_cause(e))?;
            router.route(block, None)?;
        }
    }

    let Routed {
        way_index,
        mut sort_writer,
        node_count,
        mut features_emitted,
        extent,
        mut fanout,
        node_store_stats,
        relation_plan_needed_ways,
        relation_plan_superset_ways,
        mut relations,
    } = router.finish()?;

    // Finalize way index after all way blocks are processed.
    let way_index = way_index
        .finish_writing()
        .map_err(|err| PipelineError::io("failed to finalize the way index", err))?;
    let way_count = way_counters.ways.load(Ordering::Relaxed);
    eprintln!("  Ways: {way_count}, Features so far: {features_emitted}");
    eprintln!("  Way index finalized, processing relations...");

    let relation_blocks_buffered = relations.blocks.len();
    // Relation source: the in-RAM buffer, or a filtered PBF re-read when the
    // buffer cap tripped.
    let rel_blocks_source: Box<dyn Iterator<Item = Result<PrimitiveBlock, PipelineError>> + Send> =
        if relations.spilled {
            let reader = ElementReader::from_path(&config.pbf_path)
                .map_err(|e| {
                    PipelineError::failed("relation re-read: failed to open PBF").with_cause(e)
                })?
                .with_blob_filter(BlobFilter::only_relations())
                .decode_threads(decode_threads);
            let blocks = reader.into_blocks_pipelined().map_err(|e| {
                PipelineError::failed("relation re-read: PBF pipeline start failed").with_cause(e)
            })?;
            Box::new(blocks.map(|r| {
                r.map_err(|error| {
                    PipelineError::failed("relation re-read: PBF read failed").with_cause(error)
                })
            }))
        } else {
            Box::new(std::mem::take(&mut relations.blocks).into_iter().map(Ok))
        };
    // Tail of phase12: one streamed parallel pass over all relations
    // (prepare interleaved with emit, a single end barrier), appending to
    // the same phase-wide spill coalescer. The span deliberately includes
    // the rayon fan-out - it is wall time appended to the phase either way.
    let relation_tail_busy = wait_span(&BUSY.phase12_relation_tail);
    let relation_tail = process_relation_blocks(
        rel_blocks_source,
        &way_index,
        &missing_ref_stats,
        window,
        &mut sort_writer,
        &spill,
        &mut fanout,
        config.contract.producer.fanout_caps(),
        config.contract.producer.simplify_factor(),
    )?;
    let rel_count = relation_tail.rel_count;
    features_emitted += relation_tail.features;
    let max_rel_inflight_bytes = relation_tail.max_inflight_bytes;
    drop(relation_tail_busy);
    // Every producer is done: write the coalescer's residual and adopt every
    // coalesced chunk. Producers never see a chunk write failure, so a run
    // with no other failure reports it here; a run that failed otherwise
    // returned that error first and the write failure goes unreported -
    // both are true causes, and neither path can return success.
    let paths = spill
        .finish()
        .map_err(|err| PipelineError::io("sort chunk write failed", err))?;
    sort_writer.adopt_chunk_files(paths);

    // Drop way_index to release compressed data + index memory before
    // ocean/sort/assemble phases.
    drop(way_index);

    eprintln!("  Nodes: {node_count}, Ways: {way_count}, Relations: {rel_count}");
    eprintln!("  Total features emitted: {features_emitted}");

    let data_bounds = extent.data_bounds();
    eprintln!(
        "  Data bounds (merc): x[{:.4}-{:.4}] y[{:.4}-{:.4}]",
        data_bounds.min_x, data_bounds.max_x, data_bounds.min_y, data_bounds.max_y
    );
    let stats = Phase12Stats {
        effective: crate::provenance::Effective {
            coordinate_source: if locations_on_ways {
                crate::provenance::CoordinateSource::Inline
            } else {
                crate::provenance::CoordinateSource::NodeStore
            },
            way_members: match member_source {
                MemberSource::Injected => crate::provenance::MembershipSource::InjectedV1,
                MemberSource::Plan => crate::provenance::MembershipSource::RelationScan,
            },
            shared_node_pins: match pin_source {
                PinSource::Injected => crate::provenance::PinScope::InjectedV1,
                PinSource::BlockLocal => crate::provenance::PinScope::BlockLocal,
            },
        },
        node_count,
        way_count,
        rel_count,
        node_store_stats,
        max_way_inflight_bytes: way_counters.inflight_hwm.load(Ordering::Relaxed),
        max_rel_inflight_bytes,
        relation_blocks_buffered,
        relation_blocks_bytes: relations.bytes,
        relation_blocks_spilled: relations.spilled,
        relation_plan_needed_ways,
        relation_plan_superset_ways,
        way_members_marked: way_counters.members_marked.load(Ordering::Relaxed),
        way_pins_marked: way_counters.pins_marked.load(Ordering::Relaxed),
        missing_refs: missing_ref_stats.snapshot(),
        sort_records: sort_writer.total_records(),
        sort_record_bytes: sort_writer.total_record_bytes(),
        layer_records: *sort_writer.layer_records(),
        layer_bytes: *sort_writer.layer_bytes(),
        layer_zoom_records: sort_writer.layer_zoom_records().clone(),
        layer_zoom_bytes: sort_writer.layer_zoom_bytes().clone(),
        fanout_stats: fanout,
    };
    Ok((sort_writer, data_bounds, stats))
}
