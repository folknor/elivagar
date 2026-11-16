//! What phase12 learns from the input before routing: header-declared
//! enrichments, the coordinate source, the node store, and the bounded
//! unordered block source used in locations mode.

use crate::debug::{WAIT, wait_span};
use crate::node_index::NodeStore;
use pbfhogg::{BlockType, Element, PrimitiveBlock};

use super::super::{PipelineError, RunPlan};

/// A way block plus the membership its plans resolve against.
pub(super) struct WayBlock {
    pub(super) block: PrimitiveBlock,
    pub(super) members: WayMembers,
}

/// One way block's membership source. Which variant a block gets is the
/// run's choice (the header either declares WayMembers-v1 or not); the
/// router builds it, so a way task never pairs a run mode with a block that
/// lacks what that mode needs.
pub(super) enum WayMembers {
    /// The block's own injected bitmap, validated against its ways.
    Injected(InjectedMembers),
    /// The relation plan's member-way set, shared by every block.
    Plan(std::sync::Arc<rustc_hash::FxHashSet<i64>>),
}

/// An injected way-members bitmap that has been checked against the block
/// it came with: its encoded count equals the block's decoded way count and
/// its length is that count rounded up to bytes. Only
/// `validate_and_take_members` constructs one.
pub(super) struct InjectedMembers(Box<[u8]>);

impl InjectedMembers {
    pub(super) fn bitmap(&self) -> &[u8] {
        &self.0
    }
}

/// The element kind a block is routed as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BlockKind {
    Nodes,
    Ways,
    Relations,
}

/// Classify a block for routing; `None` for a block holding no element
/// elivagar reads (no groups, or only changeset or unknown groups).
///
/// pbfhogg's `block_type()` reads the first field tag of each group, so it
/// is a label, not a kind test: a group that starts with a way can go on to
/// hold relations, and `Mixed` also stands for dense plus plain node groups,
/// a changeset group or an unknown field. The exact kind set is computed by
/// pbfhogg at block construction but is not public. So:
///
/// - a node or way label is trusted here and enforced by its consumer, which
///   walks every element anyway and refuses one of another kind (the node
///   workers and `run_way_task`), at no extra cost;
/// - a relation or `Mixed` label is walked here. Relation blocks are a small
///   share of any extract, and their consumer cannot refuse foreign elements
///   because the spilled relation re-read legitimately sees unindexed node
///   and way blobs.
///
/// A block found to hold several kinds is refused input. This is
/// conservative where a label hides the true kind - an empty dense-node
/// group in front of ways labels the block as nodes, and the node worker
/// then refuses the ways - rather than exact; exact acceptance needs the
/// kind set from pbfhogg.
pub(super) fn routed_kind(block: &PrimitiveBlock) -> Result<Option<BlockKind>, PipelineError> {
    match block.block_type() {
        BlockType::DenseNodes | BlockType::Nodes => return Ok(Some(BlockKind::Nodes)),
        BlockType::Ways => return Ok(Some(BlockKind::Ways)),
        BlockType::Empty => return Ok(None),
        BlockType::Relations | BlockType::Mixed => {}
    }
    let mut kind = None;
    for element in block.elements() {
        let this = match element {
            Element::Node(_) | Element::DenseNode(_) => BlockKind::Nodes,
            Element::Way(_) => BlockKind::Ways,
            Element::Relation(_) => BlockKind::Relations,
            _ => continue,
        };
        match kind {
            None => kind = Some(this),
            Some(seen) if seen == this => {}
            Some(seen) => return Err(mixed_kinds_error(seen, this)),
        }
    }
    Ok(kind)
}

fn mixed_kinds_error(a: BlockKind, b: BlockKind) -> PipelineError {
    PipelineError::input(format!(
        "PBF block holds more than one element kind ({a:?} and {b:?}); elivagar reads \
         one kind per block, as `pbfhogg sort` writes"
    ))
}

/// The refusal a node worker or way task raises for an element of another
/// kind in a block routed by its label.
pub(super) fn foreign_element_error(routed: BlockKind) -> PipelineError {
    PipelineError::input(format!(
        "PBF block routed as {routed:?} by its group labels holds an element of another \
         kind; elivagar reads one kind per block, as `pbfhogg sort` writes"
    ))
}

/// A decoded block, its routing kind and, for a way block under injected
/// membership, its validated bitmap; on the main locations pass, the byte
/// range of the frame it came from.
pub(super) struct Decoded {
    pub(super) block: PrimitiveBlock,
    pub(super) kind: Option<BlockKind>,
    pub(super) members: Option<InjectedMembers>,
    pub(super) range: Option<FrameRange>,
}

type DecodedBlock = Result<Decoded, PipelineError>;

/// Injected-enrichment features declared by the PBF header.
pub(super) struct InjectedFeatures {
    pub(super) members: bool,
    pub(super) pins: bool,
}

pub(super) fn detect_injected_features(
    has_members: bool,
    has_pins: bool,
    has_locations: bool,
) -> Result<InjectedFeatures, PipelineError> {
    if (has_members || has_pins) && !has_locations {
        return Err(PipelineError::input(
            "injected WayMembers-v1 or SharedNodePins-v1 requires LocationsOnWays",
        ));
    }
    Ok(InjectedFeatures {
        members: has_members,
        pins: has_pins,
    })
}

pub(in crate::pipeline) fn resolve_locations_on_ways(
    requested: bool,
    declared: bool,
) -> Result<bool, PipelineError> {
    if requested && !declared {
        return Err(PipelineError::input(
            "--locations-on-ways requires the input PBF header to declare LocationsOnWays",
        ));
    }
    Ok(declared)
}

fn validate_and_take_members(
    members: Option<(&[u8], u32)>,
    actual_way_count: usize,
) -> Result<InjectedMembers, PipelineError> {
    let (bitmap, encoded_count) = members
        .ok_or_else(|| PipelineError::input("injected way-members bitmap missing or malformed"))?;
    let actual_way_count_u64 = u64::try_from(actual_way_count)
        .map_err(|_| PipelineError::internal("decoded way count does not fit in u64"))?;
    if u64::from(encoded_count) != actual_way_count_u64 {
        return Err(PipelineError::input(format!(
            "injected way-members count mismatch: encoded {encoded_count}, decoded {actual_way_count}"
        )));
    }
    let expected_len = actual_way_count.div_ceil(8);
    if bitmap.len() != expected_len {
        return Err(PipelineError::input(format!(
            "injected way-members bitmap length mismatch: got {}, expected {expected_len}",
            bitmap.len()
        )));
    }
    Ok(InjectedMembers(bitmap.into()))
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
                return Err(PipelineError::input(format!(
                    "injected shared-node pins bitmap length mismatch for way {}: got {}, expected {expected_len}",
                    way.id(),
                    bitmap.len()
                )));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::pipeline) enum NodeStoreMode {
    None,
    Sorted,
    Flat { unsafe_override: bool },
}

fn unsorted_flat_guard_error(pbf_size: u64) -> PipelineError {
    PipelineError::input(format!(
        "PBF file is {:.1} GB but does not declare Sort.Type_then_ID.\n\
         The flat node index uses node IDs as direct file offsets and may need\n\
         many gigabytes even for a small extract. Choose an explicit option:\n\
         1. Use --force-sorted if the PBF is actually sorted (most Geofabrik extracts are)\n\
         2. Sort the PBF first with: pbfhogg sort input.pbf -o sorted.pbf\n\
         3. Alternative sorter: osmium sort input.pbf -o sorted.pbf\n\
         4. For expert debugging, use --allow-unsafe-flat-index (may require very large sparse storage)",
        pbf_size as f64 / (1024.0 * 1024.0 * 1024.0),
    ))
}

pub(in crate::pipeline) fn select_node_store_mode(
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
    if !allow_unsafe_flat_index {
        return Err(unsorted_flat_guard_error(pbf_size));
    }
    Ok(NodeStoreMode::Flat {
        unsafe_override: allow_unsafe_flat_index,
    })
}

/// Create the node store for the selected mode; `None` in locations mode,
/// where ways carry their own coordinates.
pub(super) fn create_node_store(
    mode: NodeStoreMode,
    config: &RunPlan,
    header_sorted: bool,
) -> Result<Option<NodeStore>, PipelineError> {
    Ok(match mode {
        NodeStoreMode::None => {
            eprintln!("  LocationsOnWays - skipping node store");
            None
        }
        NodeStoreMode::Sorted => {
            if config.force_sorted && !header_sorted {
                eprintln!("  --force-sorted: assuming sorted PBF (will abort if not)");
            } else {
                eprintln!("  PBF declares Sort.Type_then_ID - using compact node store");
            }
            Some(NodeStore::new_sorted())
        }
        NodeStoreMode::Flat { unsafe_override } => {
            if unsafe_override {
                eprintln!(
                    "  WARNING: unsafe flat index override enabled; bypassing the unsorted-input and flat-index-size safety guardrails"
                );
            }
            eprintln!("  PBF not sorted - using flat mmap node index");
            let path = config.tmp_dir.join("nodes.idx");
            Some(if unsafe_override {
                NodeStore::create_flat_unbounded(&path)?
            } else {
                NodeStore::create_flat(&path)?
            })
        }
    })
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
///
/// Shutting the source down drops the receiver first - which unblocks every
/// sender - and then joins the reader and decode threads: explicitly via
/// `finish` on success, or on drop after an error return, so a failed run
/// has stopped reading before it returns.
pub(super) struct UnorderedBlockSource {
    rx: Option<std::sync::mpsc::Receiver<DecodedBlock>>,
    reader_handle: Option<std::thread::JoinHandle<()>>,
    decode_handles: Vec<std::thread::JoinHandle<()>>,
    /// The main pass's own handle on the input, kept for the relation
    /// replay (see `replay`): opened before the first read, so the replay
    /// reads the file this pass read, not whatever the path names later.
    replay_file: Option<std::fs::File>,
}

/// A blob handed to a decode worker, with the byte range its whole frame
/// occupies in the input when the source tracks ranges.
type RawBlob = (pbfhogg::Blob, Option<FrameRange>);

impl UnorderedBlockSource {
    /// The main locations-mode read: every decoded block carries the byte
    /// range of its frame, so the relation tail can replay exactly the
    /// relation frames instead of re-reading the file.
    pub(super) fn spawn(
        path: &std::path::Path,
        decode_threads: usize,
        injected_members: bool,
        injected_pins: bool,
    ) -> Result<Self, PipelineError> {
        let replay_file =
            std::fs::File::open(path).map_err(|e| PipelineError::io("failed to open PBF", e))?;
        let file_len = replay_file
            .metadata()
            .map_err(|e| PipelineError::io("failed to stat PBF", e))?
            .len();
        let mut reader = pbfhogg::BlobReader::from_path(path)
            .map_err(|e| PipelineError::failed("failed to open PBF").with_cause(e))?;
        reader.set_parse_waymembers(injected_members);
        // The reader opened the path itself. The path still naming the inode
        // the replay handle holds, after that open, means both opens reached
        // the same file - so the replay reads the file the recorded ranges
        // index. In-place modification is outside this check, as it is
        // outside every input assumption here.
        same_file(&replay_file, path)?;
        let mut source = Self::spawn_reader(
            reader,
            Some(file_len),
            decode_threads,
            injected_members,
            injected_pins,
        );
        source.replay_file = Some(replay_file);
        Ok(source)
    }

    /// A source over the relation frames the main pass recorded, read back
    /// through `replay::FrameRuns`. Its blocks carry no ranges.
    pub(super) fn spawn_replay(runs: super::replay::FrameRuns, decode_threads: usize) -> Self {
        let reader = pbfhogg::BlobReader::new(std::io::BufReader::with_capacity(
            super::replay::READ_BUFFER_BYTES,
            runs,
        ));
        Self::spawn_reader(reader, None, decode_threads, false, false)
    }

    /// Reader thread plus decode pool over any blob stream. With `file_len`
    /// the reader holds one blob back so the next blob's offset closes its
    /// frame range; every blob the reader yields - header and unknown blobs
    /// included - delimits a frame, so ranges are recorded before the
    /// OsmData filter discards the others.
    fn spawn_reader<R: std::io::Read + Send + 'static>(
        reader: pbfhogg::BlobReader<R>,
        file_len: Option<u64>,
        decode_threads: usize,
        injected_members: bool,
        injected_pins: bool,
    ) -> Self {
        let raw_cap = (decode_threads * 2).max(8);
        let decoded_cap = (decode_threads * 2).max(8);
        let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<RawBlob>(raw_cap);
        let raw_rx = std::sync::Arc::new(std::sync::Mutex::new(raw_rx));
        let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedBlock>(decoded_cap);

        let reader_err_tx = decoded_tx.clone();
        let reader_handle = std::thread::spawn(move || {
            let fail = |error: PipelineError| drop(reader_err_tx.send(Err(error)));
            let send = |blob: pbfhogg::Blob, range: Option<FrameRange>| -> bool {
                if !matches!(blob.get_type(), pbfhogg::BlobType::OsmData) {
                    return true;
                }
                let _wait = wait_span(&WAIT.read_raw_send);
                raw_tx.send((blob, range)).is_ok() // false: consumer gone, stop
            };
            let mut pending: Option<(pbfhogg::Blob, u64)> = None;
            for blob_result in reader {
                let blob = match blob_result {
                    Ok(blob) => blob,
                    Err(e) => {
                        fail(PipelineError::failed("PBF read failed").with_cause(e));
                        return;
                    }
                };
                if file_len.is_none() {
                    if !send(blob, None) {
                        return;
                    }
                    continue;
                }
                let Some(start) = blob.offset().map(|offset| offset.0) else {
                    fail(PipelineError::internal(
                        "PBF reader yielded a blob without its byte offset",
                    ));
                    return;
                };
                if let Some((prev, prev_start)) = pending.take() {
                    let Some(range) = FrameRange::new(prev_start, start) else {
                        fail(PipelineError::internal("PBF blob offsets do not increase"));
                        return;
                    };
                    if !send(prev, Some(range)) {
                        return;
                    }
                }
                pending = Some((blob, start));
            }
            if let (Some((last, start)), Some(len)) = (pending, file_len) {
                // The last frame runs to the end of the file. pbfhogg reads
                // a 1-3 byte partial length prefix there as a clean end; the
                // replay can include those bytes only because this range is
                // the last one the file has.
                let Some(range) = FrameRange::new(start, len) else {
                    fail(PipelineError::internal("PBF blob offset past end of file"));
                    return;
                };
                send(last, Some(range));
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
                    let Ok((blob, range)) = blob else {
                        break; // reader done and channel drained
                    };
                    let item = decode_blob(&blob, injected_members, injected_pins)
                        .map(|decoded| Decoded { range, ..decoded });
                    let _wait = wait_span(&WAIT.read_decoded_send);
                    if tx.send(item).is_err() {
                        break; // consumer gone
                    }
                }
            }));
        }
        drop(decoded_tx);

        Self {
            rx: Some(decoded_rx),
            reader_handle: Some(reader_handle),
            decode_handles,
            replay_file: None,
        }
    }

    /// Next decoded block, or `None` once every sender is done.
    pub(super) fn recv(&self) -> Option<DecodedBlock> {
        let _wait = wait_span(&WAIT.read_decoded_recv);
        self.rx.as_ref()?.recv().ok()
    }
}

impl UnorderedBlockSource {
    /// Stop the source and join its threads; true if any of them panicked.
    fn shut_down(&mut self) -> bool {
        drop(self.rx.take());
        let mut panicked = self
            .reader_handle
            .take()
            .is_some_and(|handle| handle.join().is_err());
        for handle in self.decode_handles.drain(..) {
            panicked |= handle.join().is_err();
        }
        panicked
    }

    /// Success-path completion: a source-thread panic is reported here.
    /// Returns the main pass's input handle for the relation replay (`None`
    /// for a replay source).
    pub(super) fn finish(mut self) -> Option<std::fs::File> {
        assert!(!self.shut_down(), "PBF source thread panicked");
        self.replay_file.take()
    }
}

impl Drop for UnorderedBlockSource {
    /// Error-path cleanup: quiesce without panicking, so the run's first
    /// error is the one returned.
    fn drop(&mut self) {
        // The panic flag is deliberately discarded: see above.
        let _panicked = self.shut_down();
    }
}

fn decode_blob(blob: &pbfhogg::Blob, injected_members: bool, injected_pins: bool) -> DecodedBlock {
    let members = blob.way_members().zip(blob.way_member_count());
    let block = blob
        .to_primitiveblock()
        .map_err(|e| PipelineError::failed("PBF decode failed").with_cause(e))?;
    // The same classification routes the block, so the bitmap is validated
    // for exactly the blocks the way tasks receive.
    let kind = routed_kind(&block)?;
    let members = if injected_members && kind == Some(BlockKind::Ways) {
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
    Ok(Decoded {
        block,
        kind,
        members,
        range: None,
    })
}

/// Refuse unless `path` names the same inode as `file`.
fn same_file(file: &std::fs::File, path: &std::path::Path) -> Result<(), PipelineError> {
    use std::os::unix::fs::MetadataExt;
    let held = file
        .metadata()
        .map_err(|e| PipelineError::io("failed to stat PBF", e))?;
    let named = std::fs::metadata(path).map_err(|e| PipelineError::io("failed to stat PBF", e))?;
    if (held.dev(), held.ino()) == (named.dev(), named.ino()) {
        Ok(())
    } else {
        Err(PipelineError::failed(
            "the input PBF was replaced while it was being opened",
        ))
    }
}

/// The bytes one blob's whole frame (length prefix, `BlobHeader`, body)
/// occupies in the input: from its offset to the next frame's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FrameRange {
    pub(super) start: u64,
    pub(super) end: u64,
}

impl FrameRange {
    /// `None` unless `start < end`: a frame is never empty.
    pub(super) fn new(start: u64, end: u64) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }

    pub(super) fn len(self) -> u64 {
        self.end - self.start
    }
}

#[cfg(test)]
mod tests {
    use super::{detect_injected_features, validate_and_take_members};

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
                .bitmap(),
            &[0x05]
        );
    }
}
