//! PMTiles v3 directories: the columnar varint codec and the run types the
//! reader hands out.
//!
//! On the wire a directory entry with run length zero is a pointer to a leaf
//! directory. That sentinel lives only in [`WireEntry`], which is private to
//! the codec, the reader's traversal and the writer's spill; everything a
//! consumer sees is a [`TileRun`], whose run length is non-zero by type.

use std::io;
use std::num::NonZeroU32;

use protohoggr::{Cursor, encode_varint};

use crate::tiling::{ARCHIVE_ID_END, TileId};

/// A reference to one stored tile payload, relative to the tile data section
/// of the archive it came from. Multiple runs may point at the same blob. A
/// blob reference from one archive means nothing in another; bounds checks
/// cannot detect that mix-up, so callers must not make it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct BlobRef {
    pub offset: u64,
    pub length: u32,
}

/// One run of consecutive tile ids sharing one payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileRun {
    start: TileId,
    run_length: NonZeroU32,
    blob: BlobRef,
}

impl TileRun {
    /// A run whose ids all lie below the first z32 id, or `None`.
    pub fn new(start: TileId, run_length: NonZeroU32, blob: BlobRef) -> Option<Self> {
        let end = start.raw().checked_add(u64::from(run_length.get()))?;
        (end <= ARCHIVE_ID_END).then_some(Self {
            start,
            run_length,
            blob,
        })
    }

    pub fn start(&self) -> TileId {
        self.start
    }

    /// Exclusive end id. A raw `u64`: the end of the last run may be the
    /// first id past z31, which is not an address.
    pub fn end(&self) -> u64 {
        self.start.raw() + u64::from(self.run_length.get())
    }

    pub fn run_length(&self) -> NonZeroU32 {
        self.run_length
    }

    pub fn blob(&self) -> BlobRef {
        self.blob
    }

    pub fn contains(&self, tile_id: u64) -> bool {
        self.start.raw() <= tile_id && tile_id < self.end()
    }
}

/// Every tile run of one archive, sorted by start id and non-overlapping.
/// Only the reader constructs it, after validating the directory tree, so
/// lookups here can rely on those invariants.
#[derive(Clone, Debug, Default)]
pub struct TileRuns {
    runs: Vec<TileRun>,
}

impl TileRuns {
    /// Sort and check non-overlap. Ends are already bounded by `TileRun`.
    pub(crate) fn from_unsorted(mut runs: Vec<TileRun>) -> io::Result<Self> {
        runs.sort_unstable_by_key(|run| run.start.raw());
        let mut prev_end = 0_u64;
        for run in &runs {
            if run.start.raw() < prev_end {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "PMTiles tile runs overlap",
                ));
            }
            prev_end = run.end();
        }
        Ok(Self { runs })
    }

    pub fn as_slice(&self) -> &[TileRun] {
        &self.runs
    }

    pub fn len(&self) -> usize {
        self.runs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// The run covering `tile_id`, if any.
    pub fn covering(&self, tile_id: TileId) -> Option<&TileRun> {
        let index = self.runs.partition_point(|run| run.start <= tile_id);
        self.runs
            .get(index.checked_sub(1)?)
            .filter(|run| run.contains(tile_id.raw()))
    }

    /// The payload addressed by `tile_id`, if any.
    pub fn find(&self, tile_id: TileId) -> Option<BlobRef> {
        self.covering(tile_id).map(TileRun::blob)
    }

    /// Runs intersecting the id range `[start, end)`. A returned run may
    /// begin before `start` or end after `end`; callers clip.
    pub fn intersecting(&self, start: u64, end: u64) -> &[TileRun] {
        if start >= end {
            return &[];
        }
        let first = self.runs.partition_point(|run| run.end() <= start);
        let last = self.runs.partition_point(|run| run.start.raw() < end);
        &self.runs[first..last]
    }
}

/// One directory entry as encoded: run length zero is a leaf pointer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WireEntry {
    pub(crate) tile_id: u64,
    pub(crate) offset: u64,
    pub(crate) length: u32,
    pub(crate) run_length: u32,
}

impl From<TileRun> for WireEntry {
    fn from(run: TileRun) -> Self {
        Self {
            tile_id: run.start.raw(),
            offset: run.blob.offset,
            length: run.blob.length,
            run_length: run.run_length.get(),
        }
    }
}

/// Encode directory entries in PMTiles v3 columnar varint format.
///
/// Columns are tile ID deltas, run lengths, lengths, then offsets. Offset
/// zero means contiguous with the previous payload; other values store
/// `offset + 1` so the first entry can represent offset zero.
#[hotpath::measure]
pub(crate) fn encode_directory(entries: &[WireEntry]) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();

    let count = u64::try_from(entries.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many directory entries"))?;
    encode_varint(&mut buf, count);

    let mut prev_tile_id = 0_u64;
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 && entry.tile_id <= prev_tile_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory tile IDs must be strictly increasing",
            ));
        }
        encode_varint(&mut buf, entry.tile_id - prev_tile_id);
        prev_tile_id = entry.tile_id;
    }

    for entry in entries {
        encode_varint(&mut buf, u64::from(entry.run_length));
    }

    for entry in entries {
        encode_varint(&mut buf, u64::from(entry.length));
    }

    // Zero is reserved for contiguity, so an explicit offset stores offset+1.
    for (index, entry) in entries.iter().enumerate() {
        let contiguous = index > 0 && {
            let previous = &entries[index - 1];
            previous.offset.checked_add(u64::from(previous.length)) == Some(entry.offset)
        };
        if contiguous {
            encode_varint(&mut buf, 0);
        } else {
            let encoded_offset = entry.offset.checked_add(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "directory offset cannot be represented",
                )
            })?;
            encode_varint(&mut buf, encoded_offset);
        }
    }

    Ok(buf)
}

/// Decode a PMTiles v3 directory from its columnar varint encoding.
///
/// An empty directory (count zero) is accepted although the spec asks for at
/// least one entry: the writer emits one for an archive with no tiles, and
/// refusing it would make those archives unreadable. Zero-length payloads are
/// accepted for the same reason; zero-length leaf pointers are refused by the
/// reader's traversal, where the entry's role is known.
pub(crate) fn decode_directory(data: &[u8]) -> io::Result<Vec<WireEntry>> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
    let mut c = Cursor::new(data);

    let count = c
        .read_varint()
        .map_err(|e| invalid(format!("directory count: {e}")))?;
    let count = usize::try_from(count)
        .map_err(|_| invalid("directory count does not fit address space".to_string()))?;
    // Each entry needs four varints of at least one byte each, after the
    // count varint. Run lengths address many tiles without adding entries.
    if count > c.remaining() / 4 {
        return Err(invalid(
            "directory entry count exceeds encoded data length".to_string(),
        ));
    }

    let mut tile_ids = Vec::with_capacity(count);
    let mut prev: u64 = 0;
    for _ in 0..count {
        let delta = c
            .read_varint()
            .map_err(|e| invalid(format!("tile_id delta: {e}")))?;
        if !tile_ids.is_empty() && delta == 0 {
            return Err(invalid(
                "tile_id delta must be positive after first entry".to_string(),
            ));
        }
        prev = prev
            .checked_add(delta)
            .ok_or_else(|| invalid("tile_id delta: cumulative overflow".to_string()))?;
        // Every entry, leaf pointers included, names a tile address.
        if TileId::new(prev).is_none() {
            return Err(invalid(
                "tile_id delta: id lies beyond the last addressable PMTiles tile".to_string(),
            ));
        }
        tile_ids.push(prev);
    }

    let mut read_u32_column = |name: &str| -> io::Result<Vec<u32>> {
        let mut column = Vec::with_capacity(count);
        for _ in 0..count {
            let value = c
                .read_varint()
                .map_err(|e| invalid(format!("{name}: {e}")))?;
            column.push(u32::try_from(value).map_err(|_| invalid(format!("{name} exceeds u32")))?);
        }
        Ok(column)
    };
    let run_lengths = read_u32_column("run_length")?;
    let lengths = read_u32_column("length")?;

    let mut entries: Vec<WireEntry> = Vec::with_capacity(count);
    for i in 0..count {
        let v = c
            .read_varint()
            .map_err(|e| invalid(format!("offset: {e}")))?;
        let offset = if v == 0 {
            let Some(prev_entry) = i.checked_sub(1).map(|p| entries[p]) else {
                return Err(invalid(
                    "offset: zero sentinel is invalid for first entry".to_string(),
                ));
            };
            prev_entry
                .offset
                .checked_add(u64::from(prev_entry.length))
                .ok_or_else(|| invalid("offset: contiguous addition overflow".to_string()))?
        } else {
            v - 1
        };
        entries.push(WireEntry {
            tile_id: tile_ids[i],
            offset,
            length: lengths[i],
            run_length: run_lengths[i],
        });
    }

    Ok(entries)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn encode_directory_raw(
        tile_deltas: &[u64],
        run_lengths: &[u64],
        lengths: &[u64],
        offsets: &[u64],
    ) -> Vec<u8> {
        assert_eq!(tile_deltas.len(), run_lengths.len());
        assert_eq!(tile_deltas.len(), lengths.len());
        assert_eq!(tile_deltas.len(), offsets.len());
        let mut out = Vec::new();
        encode_varint(&mut out, tile_deltas.len() as u64);
        for column in [tile_deltas, run_lengths, lengths, offsets] {
            for &v in column {
                encode_varint(&mut out, v);
            }
        }
        out
    }

    #[test]
    fn decode_directory_rejects_truncated_stream() {
        // count=1 and one tile_id delta, but missing remaining columns.
        let mut raw = Vec::new();
        encode_varint(&mut raw, 1);
        encode_varint(&mut raw, 5);
        let err = decode_directory(&raw).expect_err("should fail on truncated directory");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_directory_rejects_zero_offset_for_first_entry() {
        let raw = encode_directory_raw(&[5], &[1], &[10], &[0]);
        let err = decode_directory(&raw).expect_err("first entry offset sentinel must fail");
        assert!(err.to_string().contains("invalid for first entry"));
    }

    #[test]
    fn decode_directory_rejects_contiguous_offset_overflow() {
        // Entry 0: explicit offset v=u64::MAX => offset=u64::MAX-1, length=10.
        // Entry 1: contiguous sentinel (v=0) => (u64::MAX-1)+10 overflows.
        let raw = encode_directory_raw(&[5, 1], &[1, 1], &[10, 1], &[u64::MAX, 0]);
        let err = decode_directory(&raw).expect_err("contiguous offset overflow should fail");
        assert!(err.to_string().contains("overflow"));
    }

    #[test]
    fn decode_directory_rejects_tile_id_delta_overflow() {
        let raw = encode_directory_raw(&[5, u64::MAX], &[1, 1], &[1, 1], &[1, 2]);
        let err = decode_directory(&raw).expect_err("tile_id overflow should fail");
        assert!(err.to_string().contains("cumulative overflow"));
    }

    #[test]
    fn decode_directory_rejects_ids_beyond_the_address_space() {
        let last = ARCHIVE_ID_END - 1;
        let raw = encode_directory_raw(&[last], &[1], &[1], &[1]);
        assert!(
            decode_directory(&raw).is_ok(),
            "the last z31 id is addressable"
        );
        // A leaf pointer (run length 0) is checked like any other entry.
        let raw = encode_directory_raw(&[last + 1], &[0], &[1], &[1]);
        let err = decode_directory(&raw).expect_err("an id past z31 must fail");
        assert!(err.to_string().contains("beyond"));
    }

    fn run(start: u64, len: u32, offset: u64) -> TileRun {
        TileRun::new(
            TileId::new(start).unwrap(),
            NonZeroU32::new(len).unwrap(),
            BlobRef { offset, length: 7 },
        )
        .unwrap()
    }

    #[test]
    fn tile_runs_reject_overlap_and_ends_past_z31() {
        assert!(TileRuns::from_unsorted(vec![run(12, 1, 200), run(10, 3, 100)]).is_err());
        assert!(TileRuns::from_unsorted(vec![run(13, 1, 200), run(10, 3, 100)]).is_ok());
        let last = TileId::new(ARCHIVE_ID_END - 1).unwrap();
        let blob = BlobRef {
            offset: 0,
            length: 1,
        };
        assert!(TileRun::new(last, NonZeroU32::new(1).unwrap(), blob).is_some());
        assert!(TileRun::new(last, NonZeroU32::new(2).unwrap(), blob).is_none());
    }

    #[test]
    fn lookups_cover_run_interiors_boundaries_and_misses() {
        let runs = TileRuns::from_unsorted(vec![run(10, 3, 100), run(20, 2, 200)]).unwrap();
        let at = |id| runs.find(TileId::new(id).unwrap()).map(|blob| blob.offset);
        assert_eq!((at(10), at(11), at(12)), (Some(100), Some(100), Some(100)));
        assert_eq!(at(20), Some(200));
        assert_eq!((at(9), at(13), at(22)), (None, None, None));
        assert_eq!(runs.intersecting(11, 21).len(), 2);
        assert_eq!(runs.intersecting(13, 20).len(), 0);
        assert_eq!(runs.intersecting(0, 11).len(), 1);
        assert!(runs.intersecting(11, 11).is_empty());
        assert!(runs.intersecting(12, 11).is_empty());
    }
}
