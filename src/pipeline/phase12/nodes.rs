//! Node workers: store puts, tagged-node feature emission and data-extent
//! tracking, fed whole node blocks by the router.
//!
//! The two constructors are the two legal shapes. The raw path runs exactly
//! ONE worker owning the sorted or flat node store, because the store needs
//! sequential put order (the raw path's ordered source guarantees block order
//! end to end). Locations mode runs a small pool with no store: an unordered
//! source and per-worker sinks make node work order-free. Records flow to the
//! shared spill coalescer, so workers never own the sort writer.

use crate::debug::{BUSY, WAIT, wait_span};
use crate::geometry;
use crate::node_index::{LatLonE7, NodeId, NodeStore};
use crate::shortbread::{self, OsmGeomType, Tags};
use crate::sort::{RecordTally, SortRecord, SpillCoalescer};
use pbfhogg::{Element, PrimitiveBlock};

use super::super::emit::{EmitCtx, FeatureGeometry, PointEmitScratch, RecordSink, emit_match};
use super::extent::CoordinateExtent;

const NODE_SINK_FLUSH_BYTES: usize = 8 * 1024 * 1024;
const NODE_BLOCK_CHANNEL_CAP: usize = 16;

/// What the node workers hand back when joined, merged over the pool.
#[derive(Default)]
pub(super) struct NodeOutcome {
    /// The raw path's store, returned by its single worker.
    pub(super) node_store: Option<NodeStore>,
    pub(super) node_count: u64,
    pub(super) features_emitted: u64,
    /// Applied to the sort writer once it is back from the way drain.
    pub(super) tally: Option<RecordTally>,
    pub(super) extent: CoordinateExtent,
}

impl NodeOutcome {
    fn absorb(&mut self, worker: NodeWorker) {
        if worker.node_store.is_some() {
            self.node_store = worker.node_store;
        }
        self.node_count += worker.node_count;
        self.features_emitted += worker.features_emitted;
        match self.tally.as_mut() {
            Some(tally) => tally.merge(&worker.sink.tally),
            None => self.tally = Some(worker.sink.tally),
        }
        self.extent.merge(worker.extent);
    }
}

/// Running node workers. Dropping it without [`NodeWorkers::join`] (an error
/// return) still closes the channel and waits for every worker, so a failed
/// run has stopped appending to the spill coalescer before it returns.
pub(super) struct NodeWorkers {
    tx: Option<std::sync::mpsc::SyncSender<PrimitiveBlock>>,
    handles: Vec<std::thread::JoinHandle<NodeWorker>>,
}

impl NodeWorkers {
    /// The raw path: one worker, owning the node store.
    pub(super) fn stored(
        store: NodeStore,
        spill: &std::sync::Arc<SpillCoalescer>,
        min_zoom: u8,
        max_zoom: u8,
    ) -> Self {
        Self::spawn(vec![Some(store)], spill, min_zoom, max_zoom)
    }

    /// Locations mode: a pool of `count` storeless workers.
    pub(super) fn inline(
        count: usize,
        spill: &std::sync::Arc<SpillCoalescer>,
        min_zoom: u8,
        max_zoom: u8,
    ) -> Self {
        Self::spawn(
            std::iter::repeat_with(|| None).take(count).collect(),
            spill,
            min_zoom,
            max_zoom,
        )
    }

    fn spawn(
        stores: Vec<Option<NodeStore>>,
        spill: &std::sync::Arc<SpillCoalescer>,
        min_zoom: u8,
        max_zoom: u8,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<PrimitiveBlock>(NODE_BLOCK_CHANNEL_CAP);
        let shared_rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
        let handles = stores
            .into_iter()
            .map(|node_store| {
                let rx = std::sync::Arc::clone(&shared_rx);
                let spill = std::sync::Arc::clone(spill);
                std::thread::spawn(move || {
                    let mut worker = NodeWorker::new(node_store, min_zoom, max_zoom);
                    worker.run(&rx, &spill);
                    worker
                })
            })
            .collect();
        Self {
            tx: Some(tx),
            handles,
        }
    }

    pub(super) fn send(&self, block: PrimitiveBlock) {
        let _wait = wait_span(&WAIT.node_block_send);
        self.tx
            .as_ref()
            .expect("node workers already joined")
            .send(block)
            .expect("node worker thread panicked");
    }

    /// Close the channel, wait for every worker, and merge what they return
    /// into `outcome`.
    pub(super) fn join(mut self, outcome: &mut NodeOutcome) {
        drop(self.tx.take());
        let _wait = wait_span(&WAIT.node_worker_join);
        // Join every worker before reporting a panic, so none is left
        // running when it propagates.
        let mut panicked = false;
        for handle in std::mem::take(&mut self.handles) {
            match handle.join() {
                Ok(worker) => outcome.absorb(worker),
                Err(_) => panicked = true,
            }
        }
        assert!(!panicked, "node worker thread panicked");
    }
}

impl Drop for NodeWorkers {
    fn drop(&mut self) {
        drop(self.tx.take());
        for handle in self.handles.drain(..) {
            // Error path only: the run is already failing, so a worker panic
            // here is not a second verdict worth aborting over.
            drop(handle.join());
        }
    }
}

/// One node worker's state, handed back when it is joined.
struct NodeWorker {
    node_store: Option<NodeStore>,
    min_zoom: u8,
    max_zoom: u8,
    node_count: u64,
    features_emitted: u64,
    extent: CoordinateExtent,
    node_records: Vec<SortRecord>,
    sink: RecordSink,
}

impl NodeWorker {
    fn new(node_store: Option<NodeStore>, min_zoom: u8, max_zoom: u8) -> Self {
        Self {
            node_store,
            min_zoom,
            max_zoom,
            node_count: 0,
            features_emitted: 0,
            extent: CoordinateExtent::default(),
            node_records: Vec::new(),
            sink: RecordSink::new(),
        }
    }

    fn run(
        &mut self,
        rx: &std::sync::Mutex<std::sync::mpsc::Receiver<PrimitiveBlock>>,
        spill: &SpillCoalescer,
    ) {
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
                Element::DenseNode(node) => self.handle(
                    node.id(),
                    node.decimicro_lat(),
                    node.decimicro_lon(),
                    node.tags(),
                ),
                Element::Node(node) => self.handle(
                    node.id(),
                    node.decimicro_lat(),
                    node.decimicro_lon(),
                    node.tags(),
                ),
                _ => {}
            });
            if self.sink.bytes() >= NODE_SINK_FLUSH_BYTES {
                spill.append(&self.sink.records, &self.sink.payload);
                self.sink.clear_payload();
            }
        }
        spill.append(&self.sink.records, &self.sink.payload);
        self.sink.clear_payload();
    }

    /// One node, dense or not: `Node` and `DenseNode` are distinct pbfhogg
    /// types with no shared trait, so the caller hands over the fields.
    fn handle<'a>(
        &mut self,
        id: i64,
        lat_e7: i32,
        lon_e7: i32,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
    ) {
        self.node_count += 1;
        if let Some(ns) = self.node_store.as_mut() {
            ns.put_coord_id(NodeId::new(id), LatLonE7::new(lat_e7, lon_e7));
        }
        self.extent.include(lat_e7, lon_e7);

        // Collecting an empty iterator does not allocate.
        let tags_vec: Vec<(&str, &str)> = tags.collect();
        if tags_vec.is_empty() {
            return;
        }
        self.node_records.clear();
        #[allow(clippy::cast_sign_loss)]
        let n = process_node(
            id as u64,
            lat_e7,
            lon_e7,
            &tags_vec,
            self.min_zoom,
            self.max_zoom,
            &mut self.node_records,
        );
        for r in self.node_records.drain(..) {
            self.sink.tally.record(r.key, r.data.len());
            self.sink
                .records
                .push((r.key, self.sink.payload.len(), r.data.len()));
            self.sink.payload.extend_from_slice(&r.data);
        }
        self.features_emitted += n;
    }
}

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

    let p = geometry::projection::project_e7(lat_e7, lon_e7);
    let ctx = EmitCtx {
        min_zoom,
        max_zoom,
        fanout_caps: &[],
        polygon_simplify_factor: 1.0,
    };
    let mut point = PointEmitScratch::new();
    let mut count = 0;
    for m in &matches {
        count += emit_match(
            osm_id,
            FeatureGeometry::Node(&p),
            m,
            &ctx,
            records,
            &mut point,
        );
    }
    count
}
