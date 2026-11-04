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
use std::sync::atomic::{AtomicU64, AtomicUsize};

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

pub type PayloadRecord = (SortKey, usize, usize);

/// Sort record counts accumulated by emitters and the chunk writer.
///
/// These arrays retain the current 32-slot stats shape because phase12 copies
/// them into `Phase12Stats`, while `FanoutStats` has a parallel 32 by 15
/// layout. Narrowing the shared per-layer stats requires migrating both stats
/// structures and the phase12 handoff together.
#[derive(Clone)]
pub(crate) struct RecordTally {
    pub(crate) layer_records: [u64; 32],
    pub(crate) layer_bytes: [u64; 32],
    pub(crate) layer_zoom_records: Box<[u64; 32 * 15]>,
    pub(crate) layer_zoom_bytes: Box<[u64; 32 * 15]>,
    pub(crate) total_records: u64,
    pub(crate) total_record_bytes: u64,
}

impl RecordTally {
    pub(crate) fn new() -> Self {
        Self {
            layer_records: [0; 32],
            layer_bytes: [0; 32],
            layer_zoom_records: Box::new([0; 32 * 15]),
            layer_zoom_bytes: Box::new([0; 32 * 15]),
            total_records: 0,
            total_record_bytes: 0,
        }
    }

    pub(crate) fn record(&mut self, key: SortKey, data_len: usize) {
        let layer = layer_from_key(key) as usize;
        self.total_records += 1;
        self.total_record_bytes += data_len as u64;
        if layer < 32 {
            self.layer_records[layer] += 1;
            self.layer_bytes[layer] += data_len as u64;
            let zoom = zoom_from_tile_id(tile_id_from_key(key)) as usize;
            if zoom < 15 {
                let idx = layer * 15 + zoom;
                self.layer_zoom_records[idx] += 1;
                self.layer_zoom_bytes[idx] += data_len as u64;
            }
        }
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        self.total_records += other.total_records;
        self.total_record_bytes += other.total_record_bytes;
        for i in 0..32 {
            self.layer_records[i] += other.layer_records[i];
            self.layer_bytes[i] += other.layer_bytes[i];
        }
        for i in 0..(32 * 15) {
            self.layer_zoom_records[i] += other.layer_zoom_records[i];
            self.layer_zoom_bytes[i] += other.layer_zoom_bytes[i];
        }
    }
}

impl Default for RecordTally {
    fn default() -> Self {
        Self::new()
    }
}

mod format;
mod key;
mod merge;
mod write;

pub use format::ChunkCompression;
use key::{
    PARTITION_SPLIT_Z, partition_next_key, record_cmp, sort_payload_records_total,
    sort_records_total,
};
pub use key::{
    SORT_PARTITIONS, SortKey, layer_from_key, make_sort_key, partition_from_key,
    partition_start_tile_id, partition_tile_range, priority_from_key, tile_id_from_key,
    zoom_from_tile_id,
};
pub use merge::{SortPartition, SortPartitionPiece, SortPartitionReader, SortReader};
pub use write::{
    SortWriter, SpillCoalescer, write_partitioned_payload_chunks, write_sorted_chunk,
    write_sorted_payload_chunk,
};

use format::*;
