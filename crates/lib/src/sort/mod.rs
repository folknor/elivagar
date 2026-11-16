// External merge sort for tile feature records.
//
// Designed for planet-scale data (~100+ GB of sort records). Records are
// buffered in memory up to a configurable chunk size, flushed as sorted chunk
// files, then merged via a k-way merge using a binary heap.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use crate::debug::{WAIT, wait_span};
use lz4_flex::frame::{FrameDecoder, FrameEncoder};

// ---------------------------------------------------------------------------
// K-way merge stats
// ---------------------------------------------------------------------------
//
// The merge is consumed lazily during assemble, so decompression work lands in
// assemble_reader_ns rather than the sort phase. These process-global atoms
// capture what that timing hides: `sort_merge_bytes` is the total decompressed
// record volume pulled through every chunk reader (per-reader local tally,
// flushed once on drop to keep the per-record path atomic-free), and
// `sort_merge_max_fanin` is the widest k-way merge (max concurrent chunk
// readers in one partition), which grows with dataset size and drives the
// per-record heap-compare cost. Flushed by emit_sort_counters after the reader
// is dropped. `sort_chunks` already reports the chunk count.
static SORT_MERGE_BYTES: AtomicU64 = AtomicU64::new(0);
static SORT_MERGE_MAX_FANIN: AtomicU64 = AtomicU64::new(0);

pub(crate) fn reset_sort_counters() {
    use std::sync::atomic::Ordering::Relaxed;
    SORT_MERGE_BYTES.store(0, Relaxed);
    SORT_MERGE_MAX_FANIN.store(0, Relaxed);
}

/// Flush accumulated k-way merge counters to the sidecar. Call after the
/// `SortReader` has been dropped (so all chunk readers have flushed their
/// tallies); a no-op when nothing was merged.
pub fn emit_sort_counters() {
    use std::sync::atomic::Ordering::Relaxed;
    let bytes = SORT_MERGE_BYTES.load(Relaxed);
    if bytes == 0 {
        return;
    }
    crate::debug::emit_counter_u64("sort_merge_bytes", bytes);
    crate::debug::emit_counter_u64("sort_merge_max_fanin", SORT_MERGE_MAX_FANIN.load(Relaxed));
}

// ---------------------------------------------------------------------------
// SortRecord
// ---------------------------------------------------------------------------

/// A record in the sort buffer: a sort key plus opaque payload bytes.
///
/// `data` must be owned because records are serialized to chunk files on
/// disk and deserialized during k-way merge. Hot producers that already write
/// direct chunks can use `write_sorted_payload_chunk` to sort arena indexes
/// without changing the chunk file format or the merge reader ownership model.
pub struct SortRecord {
    pub key: SortKey,
    pub data: Box<[u8]>,
}
const _: () = assert!(std::mem::size_of::<SortRecord>() == 24);

/// A sort record whose payload lives in a producer's arena at
/// `offset..offset + len`. `len` stays `usize`: a `u32` saves nothing after
/// alignment and would add a cast at every use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayloadRef {
    pub key: SortKey,
    pub offset: usize,
    pub len: usize,
}

impl PayloadRef {
    /// This record's payload within `arena`.
    #[inline]
    pub fn bytes(self, arena: &[u8]) -> &[u8] {
        &arena[self.offset..self.offset + self.len]
    }
}

const LAYER_ZOOM_COUNT: usize = crate::shortbread::Layer::count() * ZOOM_COUNT;

/// One Shortbread layer at one profile zoom: the index of [`PerLayerZoom`].
/// Construction is the only bounds check, so indexing is total.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LayerZoom {
    layer: crate::shortbread::Layer,
    zoom: u8,
}

impl LayerZoom {
    pub(crate) fn new(layer: crate::shortbread::Layer, zoom: u8) -> Option<Self> {
        (zoom <= MAX_ZOOM).then_some(Self { layer, zoom })
    }

    /// The layer and zoom of a record key. Keys are minted only for profile
    /// tiles (`SortKey::new` asserts it), so only the layer byte can fail.
    pub(crate) fn of_key(key: SortKey) -> Option<Self> {
        let layer = key.layer()?;
        let zoom = key.tile().zoom();
        assert!(
            zoom <= MAX_ZOOM,
            "sort keys are minted for profile zooms only"
        );
        Some(Self { layer, zoom })
    }

    pub(crate) fn layer(self) -> crate::shortbread::Layer {
        self.layer
    }

    pub(crate) fn zoom(self) -> u8 {
        self.zoom
    }

    /// Layer-major flat index, matching the sidecar counter order.
    fn index(self) -> usize {
        self.layer as usize * ZOOM_COUNT + usize::from(self.zoom)
    }
}

/// Values indexed by a Shortbread layer and zoom.
///
/// The wire layer id is the outer dimension and zoom is the inner dimension,
/// matching the existing sidecar counter order. Keeping the layer count in
/// this type prevents new layers from silently falling outside stats storage.
#[derive(Clone)]
pub(crate) struct PerLayerZoom<T> {
    values: Box<[T; LAYER_ZOOM_COUNT]>,
}

impl<T: Copy + Default> Default for PerLayerZoom<T> {
    fn default() -> Self {
        Self::from_fn(|| T::default())
    }
}

impl<T> PerLayerZoom<T> {
    pub(crate) fn from_fn(mut make: impl FnMut() -> T) -> Self {
        Self {
            values: Box::new(std::array::from_fn(|_| make())),
        }
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, T> {
        self.values.iter()
    }

    pub(crate) fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.values.iter_mut()
    }
}

impl<T> std::ops::Index<LayerZoom> for PerLayerZoom<T> {
    type Output = T;

    fn index(&self, at: LayerZoom) -> &Self::Output {
        &self.values[at.index()]
    }
}

impl<T> std::ops::IndexMut<LayerZoom> for PerLayerZoom<T> {
    fn index_mut(&mut self, at: LayerZoom) -> &mut Self::Output {
        &mut self.values[at.index()]
    }
}

/// Sort record counts accumulated by emitters and the chunk writer.
#[derive(Clone)]
pub(crate) struct RecordTally {
    pub(crate) layer_records: [u64; crate::shortbread::Layer::count()],
    pub(crate) layer_bytes: [u64; crate::shortbread::Layer::count()],
    pub(crate) layer_zoom_records: PerLayerZoom<u64>,
    pub(crate) layer_zoom_bytes: PerLayerZoom<u64>,
    pub(crate) total_records: u64,
    pub(crate) total_record_bytes: u64,
}

impl RecordTally {
    pub(crate) fn new() -> Self {
        Self {
            layer_records: [0; crate::shortbread::Layer::count()],
            layer_bytes: [0; crate::shortbread::Layer::count()],
            layer_zoom_records: PerLayerZoom::default(),
            layer_zoom_bytes: PerLayerZoom::default(),
            total_records: 0,
            total_record_bytes: 0,
        }
    }

    pub(crate) fn record(&mut self, key: SortKey, data_len: usize) {
        self.total_records += 1;
        self.total_record_bytes += data_len as u64;
        if let Some(at) = LayerZoom::of_key(key) {
            let layer = at.layer() as usize;
            self.layer_records[layer] += 1;
            self.layer_bytes[layer] += data_len as u64;
            self.layer_zoom_records[at] += 1;
            self.layer_zoom_bytes[at] += data_len as u64;
        }
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        self.total_records += other.total_records;
        self.total_record_bytes += other.total_record_bytes;
        for i in 0..crate::shortbread::Layer::count() {
            self.layer_records[i] += other.layer_records[i];
            self.layer_bytes[i] += other.layer_bytes[i];
        }
        for (left, right) in self
            .layer_zoom_records
            .iter_mut()
            .zip(other.layer_zoom_records.iter())
        {
            *left += *right;
        }
        for (left, right) in self
            .layer_zoom_bytes
            .iter_mut()
            .zip(other.layer_zoom_bytes.iter())
        {
            *left += *right;
        }
    }
}

impl Default for RecordTally {
    fn default() -> Self {
        Self::new()
    }
}

mod error;
pub use error::{ChunkStoreError, ChunkStoreErrorKind};
mod format;
mod key;
mod merge;
mod write;

use crate::tiling::{MAX_ZOOM, ZOOM_COUNT};
pub use format::ChunkCompression;
pub(crate) use format::{SortSourceSlice, SourcePosition, SourceSeek};
use key::{
    PARTITION_SPLIT_Z, partition_next_key, record_cmp, sort_payload_records_total,
    sort_records_total,
};
pub use key::{SORT_PARTITIONS, SortKey, partition_start_tile_id, partition_tile_range};
pub use merge::{SortPartition, SortPartitionReader, SortReader};
pub use write::{
    ChunkSink, SortWriter, SpillCoalescer, write_partitioned_payload_chunks, write_sorted_chunk,
    write_sorted_payload_chunk,
};

use format::*;
