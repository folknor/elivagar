//! What phase12 learns from the input before routing: header-declared
//! enrichments, the coordinate source, the node store, and the bounded
//! unordered block source used in locations mode.

use crate::debug::{WAIT, wait_span};
use crate::node_index::NodeStore;
use pbfhogg::{BlockType, Element, PrimitiveBlock};

use super::super::{PipelineError, TilegenConfig};

/// A way block plus its optional injected per-blob membership bitmap.
pub(super) struct WayBlock {
    pub(super) block: PrimitiveBlock,
    pub(super) members: Option<Box<[u8]>>,
}

type DecodedBlock = Result<(PrimitiveBlock, Option<Box<[u8]>>), PipelineError>;

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
        return Err(PipelineError(
            "injected WayMembers-v1 or SharedNodePins-v1 requires LocationsOnWays".to_string(),
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
        return Err(PipelineError(
            "--locations-on-ways requires the input PBF header to declare LocationsOnWays"
                .to_string(),
        ));
    }
    Ok(declared)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::pipeline) enum NodeStoreMode {
    None,
    Sorted,
    Flat { unsafe_override: bool },
}

fn unsorted_flat_guard_error(pbf_size: u64) -> PipelineError {
    PipelineError(format!(
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
    config: &TilegenConfig,
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
}

impl UnorderedBlockSource {
    pub(super) fn spawn(
        path: &std::path::Path,
        decode_threads: usize,
        injected_members: bool,
        injected_pins: bool,
    ) -> Result<Self, PipelineError> {
        let raw_cap = (decode_threads * 2).max(8);
        let decoded_cap = (decode_threads * 2).max(8);
        let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<pbfhogg::Blob>(raw_cap);
        let raw_rx = std::sync::Arc::new(std::sync::Mutex::new(raw_rx));
        let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedBlock>(decoded_cap);

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
                    let item = decode_blob(&blob, injected_members, injected_pins);
                    let _wait = wait_span(&WAIT.read_decoded_send);
                    if tx.send(item).is_err() {
                        break; // consumer gone
                    }
                }
            }));
        }
        drop(decoded_tx);

        Ok(Self {
            rx: Some(decoded_rx),
            reader_handle: Some(reader_handle),
            decode_handles,
        })
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
    pub(super) fn finish(mut self) {
        assert!(!self.shut_down(), "PBF source thread panicked");
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
                .as_ref(),
            &[0x05]
        );
    }
}
