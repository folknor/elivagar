// Node coordinate storage has two private backends. The public NodeStore and
// NodeStoreReader wrappers dispatch to the selected backend.
//
// The flat mmap backend currently sets no madvise hints.

use std::cell::UnsafeCell;
use std::fs::File;
use std::io;
use std::path::Path;

use memmap2::{Mmap, MmapMut};

/// An OSM node identifier. Legacy APIs still accept `i64`; the typed methods
/// use this wrapper so node IDs are distinct from other integer keys.
///
/// Negative values can be wrapped for compatibility with those legacy APIs.
/// Stores reject them on writes and return no match on reads.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeId(i64);

impl NodeId {
    pub const fn new(raw: i64) -> Self {
        Self(raw)
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }

    fn as_nonnegative_u64(self) -> Option<u64> {
        u64::try_from(self.0).ok()
    }
}

impl From<i64> for NodeId {
    fn from(raw: i64) -> Self {
        Self::new(raw)
    }
}

impl From<NodeId> for i64 {
    fn from(node_id: NodeId) -> Self {
        node_id.as_i64()
    }
}

/// A node coordinate in signed degrees scaled by 10^7, ordered latitude first.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LatLonE7 {
    pub lat: i32,
    pub lon: i32,
}

impl LatLonE7 {
    pub const fn new(lat: i32, lon: i32) -> Self {
        Self { lat, lon }
    }
}

impl From<(i32, i32)> for LatLonE7 {
    fn from((lat, lon): (i32, i32)) -> Self {
        Self::new(lat, lon)
    }
}

impl From<LatLonE7> for (i32, i32) {
    fn from(coord: LatLonE7) -> Self {
        (coord.lat, coord.lon)
    }
}

const _: () = assert!(std::mem::size_of::<LatLonE7>() == 8);

const ENTRY_SIZE: u64 = 8; // 4 bytes lat_e7 + 4 bytes lon_e7
const GROW_INCREMENT: u64 = 1_073_741_824; // 1 GB
const MAX_FLAT_INDEX_SIZE: u64 = 16 * 1024 * 1024 * 1024; // 16 GB safety cap

// XOR mask applied to stored coordinates so that (0,0) on disk means "unset"
// while a real node at lat=0, lon=0 stores as non-zero.
// 0x55555555 = 1431655765 E7 = 143.17° - outside valid latitude range [-90°, 90°],
// so no real coordinate pair can XOR to (0, 0).
#[allow(clippy::cast_possible_wrap)]
const COORD_XOR: i32 = 0x5555_5555_u32 as i32;

/// A node the store refused to record, or an operational failure growing it.
///
/// Every variant except the two I/O ones is a property of the input's node
/// IDs - negative, out of order for the sorted store, or too large for the
/// flat store's guardrail or address arithmetic - so the run classes them as
/// rejected input. The I/O variants keep their `io::Error` as the source.
#[derive(Debug)]
pub enum NodeStoreError {
    /// A negative node ID; both backends address by non-negative IDs.
    NegativeId { node_id: i64 },
    /// The sorted store saw an ID not strictly above the previous one.
    NotIncreasing { node_id: i64, previous: i64 },
    /// The flat store would need more than its safety cap for this ID.
    SafetyCap {
        node_id: i64,
        needed_bytes: u64,
        cap_bytes: u64,
    },
    /// The ID's byte offset does not fit the flat store's address space.
    OffsetOverflow { node_id: i64 },
    /// The sorted store addresses groups densely by ID, so it refuses IDs at
    /// or above `limit` rather than size its directory by them.
    IdCap { node_id: i64, limit: u64 },
    /// Extending the flat index file failed.
    Grow(io::Error),
    /// Remapping the extended flat index failed.
    Remap(io::Error),
}

impl NodeStoreError {
    /// True for the variants that reject the input's node IDs.
    #[must_use]
    pub fn is_input_rejection(&self) -> bool {
        !matches!(self, Self::Grow(_) | Self::Remap(_))
    }
}

impl std::fmt::Display for NodeStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const GB: f64 = 1024.0 * 1024.0 * 1024.0;
        match self {
            Self::NegativeId { node_id } => {
                write!(f, "node IDs must be non-negative, got {node_id}")
            }
            Self::NotIncreasing { node_id, previous } => write!(
                f,
                "sorted node store needs strictly increasing node IDs, got {node_id} after \
                 {previous}; the PBF is not actually sorted (drop --force-sorted, or sort it \
                 with `pbfhogg sort input.pbf -o sorted.pbf`)"
            ),
            #[allow(clippy::cast_precision_loss)]
            Self::SafetyCap {
                node_id,
                needed_bytes,
                cap_bytes,
            } => write!(
                f,
                "flat node index safety cap exceeded: requested {:.1} GB for node_id={node_id} \
                 (cap: {:.1} GB). Input is likely unsorted. Use a sorted PBF, run \
                 `pbfhogg sort input.pbf -o sorted.pbf`, or use --force-sorted only if the PBF \
                 is actually sorted.",
                *needed_bytes as f64 / GB,
                *cap_bytes as f64 / GB,
            ),
            Self::OffsetOverflow { node_id } => {
                write!(f, "node_id={node_id} overflows the flat node index offset")
            }
            Self::IdCap { node_id, limit } => write!(
                f,
                "node_id={node_id} is beyond the sorted node store's range (IDs below {limit})"
            ),
            Self::Grow(_) => f.write_str("failed to grow the flat node index file"),
            Self::Remap(_) => f.write_str("failed to remap the flat node index"),
        }
    }
}

impl std::error::Error for NodeStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Grow(err) | Self::Remap(err) => Some(err),
            _ => None,
        }
    }
}

struct FlatNodeIndexInner {
    file: File,
    mmap: MmapMut,
    file_len: u64,
    max_file_size: Option<u64>,
    node_count: u64,
}

impl FlatNodeIndexInner {
    /// Create a new writable node index file at `path`.
    ///
    /// The flat mmap backend sets no madvise hints.
    fn create(path: &Path) -> io::Result<Self> {
        Self::create_internal(path, Some(MAX_FLAT_INDEX_SIZE))
    }

    /// Create a new writable node index file at `path` with no safety cap.
    ///
    /// This is intentionally unsafe and should only be used when callers
    /// explicitly opt in to bypass guardrails.
    fn create_unbounded(path: &Path) -> io::Result<Self> {
        Self::create_internal(path, None)
    }

    fn create_internal(path: &Path, max_file_size: Option<u64>) -> io::Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        let file_len = GROW_INCREMENT;
        file.set_len(file_len)?;

        let mmap = unsafe { MmapMut::map_mut(&file)? };

        Ok(FlatNodeIndexInner {
            file,
            mmap,
            file_len,
            max_file_size,
            node_count: 0,
        })
    }

    /// Write coordinates for a node. Grows the file if needed.
    /// Negative node IDs are invalid and rejected, matching `SortedNodeStore`.
    /// Write coordinates using a typed node identifier.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn put_id(&mut self, node_id: NodeId, coord: LatLonE7) -> Result<(), NodeStoreError> {
        let raw = node_id.as_i64();
        let id = node_id
            .as_nonnegative_u64()
            .ok_or(NodeStoreError::NegativeId { node_id: raw })?;
        let offset = id
            .checked_mul(ENTRY_SIZE)
            .ok_or(NodeStoreError::OffsetOverflow { node_id: raw })?;
        let needed = offset
            .checked_add(ENTRY_SIZE)
            .ok_or(NodeStoreError::OffsetOverflow { node_id: raw })?;

        if let Some(max_file_size) = self.max_file_size
            && needed > max_file_size
        {
            return Err(NodeStoreError::SafetyCap {
                node_id: raw,
                needed_bytes: needed,
                cap_bytes: max_file_size,
            });
        }

        if needed > self.file_len {
            // Grow in 1 GB increments until large enough.
            let missing = needed - self.file_len;
            let mut increments = missing / GROW_INCREMENT;
            if !missing.is_multiple_of(GROW_INCREMENT) {
                increments += 1;
            }
            let new_len = increments
                .checked_mul(GROW_INCREMENT)
                .and_then(|growth| self.file_len.checked_add(growth))
                .ok_or(NodeStoreError::OffsetOverflow { node_id: raw })?;
            // The mapping must cover the whole slot, so an ID past the
            // address space is input the flat store cannot represent.
            usize::try_from(new_len)
                .map_err(|_| NodeStoreError::OffsetOverflow { node_id: raw })?;
            self.file.set_len(new_len).map_err(NodeStoreError::Grow)?;
            self.mmap = unsafe { MmapMut::map_mut(&self.file).map_err(NodeStoreError::Remap)? };

            self.file_len = new_len;
        }

        let off = offset as usize;
        self.mmap[off..off + 4].copy_from_slice(&(coord.lat ^ COORD_XOR).to_le_bytes());
        self.mmap[off + 4..off + 8].copy_from_slice(&(coord.lon ^ COORD_XOR).to_le_bytes());
        self.node_count += 1;
        Ok(())
    }

    /// Read coordinates for a node. Returns None if entry is unset (all zeros).
    /// Read coordinates using a typed node identifier.
    fn get_id(&self, node_id: NodeId) -> Option<LatLonE7> {
        get_from_mmap(&self.mmap, self.file_len, node_id)
    }

    /// Convert to a read-only reader after all nodes have been written.
    /// Consumes the writable index - no more `put()` calls are possible.
    ///
    /// The returned `NodeIndexReader` wraps a read-only `Mmap` which is `Sync`,
    /// allowing safe concurrent reads from rayon worker threads.
    fn into_reader(self) -> io::Result<FlatNodeIndexReaderInner> {
        let file_len = self.file_len;
        let node_count = self.node_count;
        let mmap = self.mmap.make_read_only()?;
        Ok(FlatNodeIndexReaderInner {
            mmap,
            file_len,
            node_count,
        })
    }
}

/// Shared get logic for both NodeIndex and NodeIndexReader.
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::unwrap_used
)]
fn get_from_mmap(mmap: &[u8], file_len: u64, node_id: NodeId) -> Option<LatLonE7> {
    let offset = node_id.as_nonnegative_u64()?.checked_mul(ENTRY_SIZE)?;
    let needed = offset.checked_add(ENTRY_SIZE)?;

    if needed > file_len {
        return None;
    }

    let off = offset as usize;
    // Infallible: slices are exactly 4 bytes by construction.
    let lat_raw = i32::from_le_bytes(mmap[off..off + 4].try_into().unwrap());
    let lon_raw = i32::from_le_bytes(mmap[off + 4..off + 8].try_into().unwrap());

    if lat_raw == 0 && lon_raw == 0 {
        None
    } else {
        Some(LatLonE7::new(lat_raw ^ COORD_XOR, lon_raw ^ COORD_XOR))
    }
}

/// Read-only node coordinate index. Safe for concurrent reads from rayon threads
/// (`Mmap` is `Sync + Send`).
///
/// Created from `NodeIndex::into_reader()` after all nodes have been written.
struct FlatNodeIndexReaderInner {
    mmap: Mmap,
    file_len: u64,
    node_count: u64,
}

impl FlatNodeIndexReaderInner {
    /// Read coordinates using a typed node identifier.
    fn get_id(&self, node_id: NodeId) -> Option<LatLonE7> {
        get_from_mmap(&self.mmap, self.file_len, node_id)
    }
}

// ---------------------------------------------------------------------------
// SortedNodeStore - compact hierarchical store for sorted PBF files
// ---------------------------------------------------------------------------

// Tried NODES_PER_CHUNK = 512 (halving chunk count to reduce decompress_chunk calls).
// Result: 6.5% fewer decompress calls but each 2x slower (decompressing 512 vs 256 nodes),
// blob grew 270→294 MB increasing DRAM pressure. Net PBF phase +800ms regression. Reverted.
const NODES_PER_CHUNK: usize = 256;
const NODES_PER_GROUP: u64 = 256 * 256; // 65536

/// Exclusive upper bound on sorted-store node IDs. The group directory is a
/// dense `Vec<Option<Box<Group>>>` indexed by `id / NODES_PER_GROUP`, so its
/// length follows the largest ID, not the node count: one huge ID in a tiny
/// file would otherwise allocate proportionally or abort. 2^40 caps the
/// directory at 2^24 slots, a 128 MiB request on 64-bit targets, while
/// sitting about two orders of magnitude above the OSM node ID range
/// (around 2^34 in 2026). Dense indexing keeps the reader's lookup, the
/// way-resolution hot path, a single index.
const SORTED_NODE_ID_LIMIT: u64 = 1 << 40;
const BITMASK_BYTES: usize = 32; // 256 bits

/// Set bit `pos` (0..255) in a 256-bit mask.
#[inline]
fn set_bit(mask: &mut [u8; BITMASK_BYTES], pos: u8) {
    mask[pos as usize / 8] |= 1 << (pos % 8);
}

/// Test whether bit `pos` (0..255) is set in a 256-bit mask.
#[inline]
fn test_bit(mask: &[u8; BITMASK_BYTES], pos: u8) -> bool {
    mask[pos as usize / 8] & (1 << (pos % 8)) != 0
}

/// Count set bits before position `pos` in a 256-bit mask.
/// This gives the index into the packed array for the element at `pos`.
#[inline]
fn count_bits_before(mask: &[u8; BITMASK_BYTES], pos: u8) -> usize {
    let byte_idx = pos as usize / 8;
    let bit_idx = pos % 8;
    let full: usize = mask[..byte_idx]
        .iter()
        .map(|b| b.count_ones() as usize)
        .sum();
    let partial = (mask[byte_idx] & ((1u8 << bit_idx) - 1)).count_ones() as usize;
    full + partial
}

/// Count total set bits in a 256-bit mask.
#[inline]
#[allow(clippy::cast_possible_truncation)]
fn count_set_bits(mask: &[u8; BITMASK_BYTES]) -> u16 {
    mask.iter().map(|b| u16::from(b.count_ones() as u8)).sum()
}

// ---------------------------------------------------------------------------
// Exact-size bitpacking - pack/unpack N values at a given bit-width.
// Replaces BitPacker4x (128-value blocks with padding) so every chunk
// compresses at its actual size.
// ---------------------------------------------------------------------------

mod codec {
    use super::{LatLonE7, NODES_PER_CHUNK};

    /// Pack `values` at `bit_width` bits each, appending to `dest`.
    /// Output size: ceiling of values.len() * bit_width / 8 bytes.
    #[allow(clippy::cast_possible_truncation)]
    fn bitpack_values_into(values: &[u32], bit_width: u8, dest: &mut Vec<u8>) {
        if bit_width == 0 {
            return;
        }
        let bw = u32::from(bit_width);
        let mask = if bw >= 32 { u64::MAX } else { (1u64 << bw) - 1 };
        let mut accumulator: u64 = 0;
        let mut bits_in_acc: u32 = 0;
        for &v in values {
            accumulator |= (u64::from(v) & mask) << bits_in_acc;
            bits_in_acc += bw;
            while bits_in_acc >= 8 {
                dest.push(accumulator as u8);
                accumulator >>= 8;
                bits_in_acc -= 8;
            }
        }
        if bits_in_acc > 0 {
            dest.push(accumulator as u8);
        }
    }

    /// Unpack `n` values at `bit_width` bits each from `packed` into `out[..n]`.
    ///
    /// Uses a single u64 unaligned read per value (fast path) to avoid byte-by-byte
    /// assembly. Tried accumulator-based approach (refill loop with `while bits_in_acc < bw`):
    /// regressed synthetic by ~10% due to branch misprediction on the refill loop.
    /// Doesn't matter anyway - decompress_chunk is DRAM-latency-bound on real data
    /// (820ns avg on 270 MB blob vs 25ns synthetic with L1-hot data).
    #[allow(
        clippy::cast_possible_truncation,
        clippy::unwrap_used,
        clippy::needless_range_loop
    )]
    fn bitunpack_values(packed: &[u8], n: usize, bit_width: u8, out: &mut [u32]) {
        if bit_width == 0 {
            for o in &mut out[..n] {
                *o = 0;
            }
            return;
        }
        let bw = u32::from(bit_width);
        let mask = if bw >= 32 { u64::MAX } else { (1u64 << bw) - 1 };
        let mut bit_offset: usize = 0;
        for i in 0..n {
            let byte_idx = bit_offset / 8;
            let bit_idx = bit_offset % 8;
            // Read a u64 covering the bits we need. Fast path: single unaligned load
            // when 8 or more bytes remain. Slow path: byte-by-byte near end of buffer.
            let raw = if byte_idx + 8 <= packed.len() {
                u64::from_le_bytes(packed[byte_idx..byte_idx + 8].try_into().unwrap())
            } else {
                let mut r: u64 = 0;
                for b in byte_idx..packed.len() {
                    r |= u64::from(packed[b]) << ((b - byte_idx) * 8);
                }
                r
            };
            out[i] = ((raw >> bit_idx) & mask) as u32;
            bit_offset += bw as usize;
        }
    }

    /// Compute the minimum number of bits needed to represent the max value.
    fn required_bits(max_val: u32) -> u8 {
        if max_val == 0 {
            return 0;
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            (32 - max_val.leading_zeros()) as u8
        }
    }

    /// Compress coordinates with exact-size FOR and append to `dest`.
    /// Format: [lat_min:4][lon_min:4][lat_bits:1][lon_bits:1][packed lat][packed lon]
    /// Packs exactly N values - no padding to block boundaries.
    #[hotpath::measure]
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn compress_coords_into(
        coords: &[LatLonE7],
        lat_offsets: &mut Vec<u32>,
        lon_offsets: &mut Vec<u32>,
        dest: &mut Vec<u8>,
    ) {
        // Compute min values and offsets
        let lat_min = coords.iter().map(|coord| coord.lat).min().unwrap_or(0);
        let lon_min = coords.iter().map(|coord| coord.lon).min().unwrap_or(0);

        // Offsets are unsigned and can span the full i32 coordinate range.
        // Use modular arithmetic for the intermediate signed representation.
        #[allow(clippy::cast_sign_loss)]
        let lat_max_offset = coords
            .iter()
            .map(|coord| coord.lat.wrapping_sub(lat_min) as u32)
            .max()
            .unwrap_or(0);
        #[allow(clippy::cast_sign_loss)]
        let lon_max_offset = coords
            .iter()
            .map(|coord| coord.lon.wrapping_sub(lon_min) as u32)
            .max()
            .unwrap_or(0);

        let lat_bits = required_bits(lat_max_offset);
        let lon_bits = required_bits(lon_max_offset);

        // Write 10-byte header
        dest.extend_from_slice(&lat_min.to_le_bytes());
        dest.extend_from_slice(&lon_min.to_le_bytes());
        dest.push(lat_bits);
        dest.push(lon_bits);

        // Build offset arrays and pack
        lat_offsets.clear();
        lon_offsets.clear();
        #[allow(clippy::cast_sign_loss)]
        for coord in coords {
            lat_offsets.push(coord.lat.wrapping_sub(lat_min) as u32);
            lon_offsets.push(coord.lon.wrapping_sub(lon_min) as u32);
        }

        bitpack_values_into(lat_offsets, lat_bits, dest);
        bitpack_values_into(lon_offsets, lon_bits, dest);
    }

    /// Decompress all coordinates from a chunk into the output slice.
    /// `n` comes from the chunk mask; `compressed` and `packed` come from its header.
    /// For compressed: [lat_min:4][lon_min:4][lat_bits:1][lon_bits:1][packed lat][packed lon]
    /// For raw: sequential (i32, i32) pairs.
    ///
    /// This is the dominant cost in the PBF read phase. On Denmark (dm6):
    ///   ~24M calls, 820ns avg, ~20s cumulative (with 4-entry LRU cache).
    ///   The 270 MB blob doesn't fit in L2 cache, so each miss fetches from DRAM.
    ///   Synthetic benchmarks show 25ns/call (L1-hot) - NOT representative.
    ///   Compute optimizations (accumulator unpacking, etc.) don't help because
    ///   the CPU is waiting on memory, not on arithmetic.
    #[hotpath::measure]
    #[allow(clippy::unwrap_used, clippy::needless_range_loop)]
    pub(super) fn decompress_chunk(
        n: usize,
        compressed: bool,
        packed: &[u8],
        out: &mut [LatLonE7],
    ) {
        if !compressed {
            // Raw (i32, i32) pairs - no FOR encoding
            for i in 0..n {
                let off = i * 8;
                let lat = i32::from_le_bytes(packed[off..off + 4].try_into().unwrap());
                let lon = i32::from_le_bytes(packed[off + 4..off + 8].try_into().unwrap());
                out[i] = LatLonE7::new(lat, lon);
            }
            return;
        }

        // Read 10-byte header
        let lat_min = i32::from_le_bytes(packed[0..4].try_into().unwrap());
        let lon_min = i32::from_le_bytes(packed[4..8].try_into().unwrap());
        let lat_bits = packed[8];
        let lon_bits = packed[9];

        // Unpack lat offsets
        let lat_packed_bytes = (n * lat_bits as usize).div_ceil(8);
        let lat_packed = &packed[10..10 + lat_packed_bytes];
        let mut lat_buf = [0u32; NODES_PER_CHUNK];
        bitunpack_values(lat_packed, n, lat_bits, &mut lat_buf);

        // Unpack lon offsets
        let lon_packed = &packed[10 + lat_packed_bytes..];
        let mut lon_buf = [0u32; NODES_PER_CHUNK];
        bitunpack_values(lon_packed, n, lon_bits, &mut lon_buf);

        // Reconstruct coordinates
        for i in 0..n {
            #[allow(clippy::cast_possible_wrap)]
            {
                out[i] = LatLonE7::new(
                    lat_min.wrapping_add(lat_buf[i] as i32),
                    lon_min.wrapping_add(lon_buf[i] as i32),
                );
            }
        }
    }
}

/// Per-group flat blob layout. Each chunk is stored inline as:
/// `[node_mask: 32 bytes][flags_and_len: u16][packed_data: len bytes]`
/// where flags_and_len bit 15 = compressed, bits 0-14 = packed_data length.
/// No separate struct per chunk - everything lives in `data`.
const CHUNK_HEADER_SIZE: usize = BITMASK_BYTES + 2; // 34 bytes

/// Encode flags_and_len: bit 15 = compressed, bits 0-14 = packed_len.
#[inline]
#[allow(clippy::cast_possible_truncation)]
fn encode_flags_and_len(compressed: bool, packed_len: usize) -> u16 {
    debug_assert!(
        packed_len <= 0x7FFF,
        "packed_len {packed_len} exceeds 15-bit limit"
    );
    let flags = if compressed { 0x8000u16 } else { 0u16 };
    flags | packed_len as u16
}

struct Group {
    chunk_mask: [u8; BITMASK_BYTES],
    data: Box<[u8]>, // flat blob: all chunks' headers + packed data
}

use self::codec::{compress_coords_into, decompress_chunk};

// ---------------------------------------------------------------------------
// Flat blob navigation
// ---------------------------------------------------------------------------

/// Result of locating a chunk within a group's flat blob.
struct ChunkRef<'a> {
    node_mask: &'a [u8; BITMASK_BYTES],
    compressed: bool,
    packed: &'a [u8],
}

/// Scan the group's flat data blob to find the chunk at `chunk_idx`
/// (0-based index among present chunks in this group).
///
/// Linear scan through variable-length chunks. Tried adding an offset table
/// (pre-computed byte offsets per chunk) to make this O(1) - reverted because
/// cache hit rate is ~76%, so this function only runs on ~24% of lookups,
/// and Denmark averages ~7 chunks/group so the scan is short anyway.
#[hotpath::measure]
#[allow(clippy::unwrap_used)]
fn find_chunk_in_blob(data: &[u8], chunk_idx: usize) -> ChunkRef<'_> {
    let mut offset = 0;
    for _ in 0..chunk_idx {
        // Skip: node_mask (32) + flags_and_len (2) + packed data
        let fl = u16::from_le_bytes([
            data[offset + BITMASK_BYTES],
            data[offset + BITMASK_BYTES + 1],
        ]);
        let packed_len = (fl & 0x7FFF) as usize;
        offset += CHUNK_HEADER_SIZE + packed_len;
    }
    let node_mask: &[u8; BITMASK_BYTES] = data[offset..offset + BITMASK_BYTES].try_into().unwrap();
    let fl = u16::from_le_bytes([
        data[offset + BITMASK_BYTES],
        data[offset + BITMASK_BYTES + 1],
    ]);
    let compressed = fl & 0x8000 != 0;
    let packed_len = (fl & 0x7FFF) as usize;
    let packed_start = offset + CHUNK_HEADER_SIZE;
    ChunkRef {
        node_mask,
        compressed,
        packed: &data[packed_start..packed_start + packed_len],
    }
}

// ---------------------------------------------------------------------------
// Lookup functions
// ---------------------------------------------------------------------------

/// Single cache entry: one decompressed chunk.
struct CacheEntry {
    reader_id: u64,
    group_id: usize,
    chunk_idx: usize,
    node_mask: [u8; BITMASK_BYTES],
    coords: [LatLonE7; NODES_PER_CHUNK],
    count: u16, // 0 = empty
}

impl CacheEntry {
    fn new() -> Self {
        CacheEntry {
            reader_id: 0,
            group_id: usize::MAX,
            chunk_idx: usize::MAX,
            node_mask: [0u8; BITMASK_BYTES],
            coords: [LatLonE7::new(0, 0); NODES_PER_CHUNK],
            count: 0,
        }
    }
}

/// Thread-local decompression cache with LRU eviction.
///
/// Started with 1 entry - ways that span 2 nearby chunks caused constant
/// eviction (ping-ponging). 4 entries cut decompress_chunk calls by 17%
/// (28.7M→23.8M on Denmark) and avg latency by 42% (1.41µs→820ns).
/// Total decompress_chunk time: 40.4s→19.5s (-52%). PBF phase -800ms.
///
/// 8 entries improves hit rate for larger ways (20+ nodes spanning 5+ chunks)
/// and inter-way cross-pollination within PBF blocks. Memory cost: ~17 KB/thread.
///
/// LRU policy: on hit, swap entry to front; on miss, evict last entry.
/// `entries.swap()` avoids memcpy of the large coords arrays.
const CACHE_ENTRIES: usize = 8;

struct DecompressCache {
    entries: [CacheEntry; CACHE_ENTRIES],
}

impl DecompressCache {
    fn new() -> Self {
        DecompressCache {
            entries: [
                CacheEntry::new(),
                CacheEntry::new(),
                CacheEntry::new(),
                CacheEntry::new(),
                CacheEntry::new(),
                CacheEntry::new(),
                CacheEntry::new(),
                CacheEntry::new(),
            ],
        }
    }
}

// UnsafeCell instead of RefCell: eliminates runtime borrow-check overhead on every
// cache access (~48M calls on Denmark). Measured -3.1% on synthetic way-like lookups.
// SAFETY: thread_local! guarantees single-threaded access - no concurrent borrows.
thread_local! {
    static DECOMPRESS_CACHE: UnsafeCell<DecompressCache> = UnsafeCell::new(DecompressCache::new());
}

/// Look up a node within a finalized Group, using the thread-local cache.
/// On cache hit, skips the blob scan entirely - uses cached node_mask.
///
/// No #[hotpath::measure] - at ~48M calls on Denmark, the two clock_gettime
/// syscalls per call added >50% overhead. Removed in 178ca73.
#[inline]
fn get_from_group_cached(
    group: &Group,
    reader_id: u64,
    group_id: usize,
    chunk_id: u8,
    node_in_chunk: u8,
) -> Option<LatLonE7> {
    if !test_bit(&group.chunk_mask, chunk_id) {
        return None;
    }
    let chunk_idx = count_bits_before(&group.chunk_mask, chunk_id);

    // SAFETY: thread_local! guarantees single-threaded access - no concurrent borrows possible.
    DECOMPRESS_CACHE.with(|cell| {
        let cache = unsafe { &mut *cell.get() };

        // Search cache entries for a hit
        for i in 0..CACHE_ENTRIES {
            let entry = &cache.entries[i];
            if entry.reader_id == reader_id
                && entry.group_id == group_id
                && entry.chunk_idx == chunk_idx
                && entry.count != 0
            {
                if !test_bit(&entry.node_mask, node_in_chunk) {
                    return None;
                }
                let node_idx = count_bits_before(&entry.node_mask, node_in_chunk);
                let result = entry.coords[node_idx];
                // LRU promote: swap hit entry to front (no memcpy of large coords arrays)
                if i > 0 {
                    cache.entries.swap(0, i);
                }
                return Some(result);
            }
        }

        // Cache miss - scan blob, decompress, evict LRU (last) entry
        let chunk = find_chunk_in_blob(&group.data, chunk_idx);
        if !test_bit(chunk.node_mask, node_in_chunk) {
            return None;
        }
        let node_idx = count_bits_before(chunk.node_mask, node_in_chunk);

        // Evict last entry: swap it to front, then overwrite
        cache.entries.swap(0, CACHE_ENTRIES - 1);
        let entry = &mut cache.entries[0];
        let count = count_set_bits(chunk.node_mask);
        decompress_chunk(
            count as usize,
            chunk.compressed,
            chunk.packed,
            &mut entry.coords,
        );
        entry.node_mask = *chunk.node_mask;
        entry.reader_id = reader_id;
        entry.group_id = group_id;
        entry.chunk_idx = chunk_idx;
        entry.count = count;
        Some(entry.coords[node_idx])
    })
}

/// Look up a single node from a chunk in a blob (no cache, for builder/test path).
#[inline]
fn get_from_blob_chunk(data: &[u8], chunk_idx: usize, node_in_chunk: u8) -> Option<LatLonE7> {
    let chunk = find_chunk_in_blob(data, chunk_idx);
    if !test_bit(chunk.node_mask, node_in_chunk) {
        return None;
    }
    let node_idx = count_bits_before(chunk.node_mask, node_in_chunk);
    let mut coords = [LatLonE7::new(0, 0); NODES_PER_CHUNK];
    let count = count_set_bits(chunk.node_mask);
    decompress_chunk(count as usize, chunk.compressed, chunk.packed, &mut coords);
    Some(coords[node_idx])
}

/// Look up a node within a finalized Group (no cache, for builder/test path).
#[inline]
fn get_from_group(group: &Group, chunk_id: u8, node_in_chunk: u8) -> Option<LatLonE7> {
    if !test_bit(&group.chunk_mask, chunk_id) {
        return None;
    }
    let chunk_idx = count_bits_before(&group.chunk_mask, chunk_id);
    get_from_blob_chunk(&group.data, chunk_idx, node_in_chunk)
}

/// Compact node coordinate store for sorted PBF files with FOR compression.
///
/// Requires node IDs to be inserted in strictly ascending order (guaranteed
/// by PBF `Sort.Type_then_ID`). Builds a 3-level hierarchy: groups (64K
/// nodes each) → chunks (256 nodes each) → individual nodes. Each level
/// uses a 256-bit bitmask for O(1) presence test and popcount-based indexing.
///
/// Coordinates within each chunk are FOR-compressed using exact-size
/// bitpacking (no padding to block boundaries). Decompression uses a
/// thread-local cache to amortize cost across consecutive lookups.
struct SortedNodeStoreInner {
    /// Completed groups, indexed by group_id. None = no nodes in that group.
    groups: Vec<Option<Box<Group>>>,

    // Builder state for the group being accumulated.
    current_group_id: u64,
    current_chunk_mask: [u8; BITMASK_BYTES],
    current_group_data: Vec<u8>, // flat blob being built for current group
    compress_buf: Vec<u8>,       // scratch buffer for compression, reused
    scratch_lat_offsets: Vec<u32>, // scratch buffer for compress_coords_into, reused
    scratch_lon_offsets: Vec<u32>, // scratch buffer for compress_coords_into, reused

    // Builder state for the chunk being accumulated.
    current_chunk_id: u8,
    current_node_mask: [u8; BITMASK_BYTES],
    current_coords: Vec<LatLonE7>,

    last_node_id: Option<NodeId>,
    node_count: u64,
}

impl Default for SortedNodeStoreInner {
    fn default() -> Self {
        Self::new()
    }
}

impl SortedNodeStoreInner {
    fn new() -> Self {
        SortedNodeStoreInner {
            groups: Vec::new(),
            current_group_id: 0,
            current_chunk_mask: [0u8; BITMASK_BYTES],
            current_group_data: Vec::new(),
            compress_buf: Vec::new(),
            scratch_lat_offsets: Vec::with_capacity(NODES_PER_CHUNK),
            scratch_lon_offsets: Vec::with_capacity(NODES_PER_CHUNK),
            current_chunk_id: 0,
            current_node_mask: [0u8; BITMASK_BYTES],
            current_coords: Vec::with_capacity(NODES_PER_CHUNK),
            last_node_id: None,
            node_count: 0,
        }
    }

    /// Store typed coordinates. Node IDs MUST be strictly increasing.
    #[hotpath::measure]
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn put_id(&mut self, node_id: NodeId, coord: LatLonE7) -> Result<(), NodeStoreError> {
        let id = node_id
            .as_nonnegative_u64()
            .ok_or(NodeStoreError::NegativeId {
                node_id: node_id.as_i64(),
            })?;
        if let Some(previous) = self.last_node_id
            && node_id <= previous
        {
            return Err(NodeStoreError::NotIncreasing {
                node_id: node_id.as_i64(),
                previous: previous.as_i64(),
            });
        }
        if id >= SORTED_NODE_ID_LIMIT {
            return Err(NodeStoreError::IdCap {
                node_id: node_id.as_i64(),
                limit: SORTED_NODE_ID_LIMIT,
            });
        }
        self.last_node_id = Some(node_id);
        self.node_count += 1;

        let group_id = id / NODES_PER_GROUP;
        let chunk_id = ((id % NODES_PER_GROUP) / NODES_PER_CHUNK as u64) as u8;
        let node_in_chunk = (id % NODES_PER_CHUNK as u64) as u8;

        if self.node_count == 1 {
            // First node: initialize builder state.
            self.grow_groups(group_id as usize);
            self.current_group_id = group_id;
            self.current_chunk_id = chunk_id;
        } else if group_id != self.current_group_id {
            // New group: flush current chunk + group.
            self.flush_chunk();
            self.flush_group();
            self.grow_groups(group_id as usize);
            self.current_group_id = group_id;
            self.current_chunk_mask = [0u8; BITMASK_BYTES];
            // current_group_data already cleared by flush_group
            self.current_chunk_id = chunk_id;
            self.current_node_mask = [0u8; BITMASK_BYTES];
            self.current_coords.clear();
        } else if chunk_id != self.current_chunk_id {
            // Same group, new chunk: flush current chunk.
            self.flush_chunk();
            self.current_chunk_id = chunk_id;
            self.current_node_mask = [0u8; BITMASK_BYTES];
            self.current_coords.clear();
        }

        set_bit(&mut self.current_node_mask, node_in_chunk);
        self.current_coords.push(coord);
        Ok(())
    }

    /// Extend the directory to hold `group_id`. Growth stays geometric, so a
    /// sorted file adding one group at a time does not reallocate per group,
    /// but the request is clamped at the slot count `SORTED_NODE_ID_LIMIT`
    /// allows rather than doubling past it.
    #[allow(clippy::cast_possible_truncation)]
    fn grow_groups(&mut self, group_id: usize) {
        const MAX_SLOTS: usize = (SORTED_NODE_ID_LIMIT / NODES_PER_GROUP) as usize;
        let target = group_id + 1;
        let len = self.groups.len();
        if len < target {
            if self.groups.capacity() < target {
                let wanted = target.max(self.groups.capacity().saturating_mul(2).min(MAX_SLOTS));
                self.groups.reserve_exact(wanted - len);
            }
            self.groups.resize_with(target, || None);
        }
    }

    // Offset scratch buffers are hoisted onto the struct to avoid alloc/dealloc
    // per chunk (~205K calls on Denmark). Hoisting gave -5.3% build time,
    // -6.8% way-like lookup time (9e014d1).
    #[hotpath::measure]
    fn flush_chunk(&mut self) {
        if self.current_coords.is_empty() {
            return;
        }
        set_bit(&mut self.current_chunk_mask, self.current_chunk_id);

        let n = self.current_coords.len();
        let raw_size = n * 8; // 4 bytes lat + 4 bytes lon

        // Compress into scratch buffer
        self.compress_buf.clear();
        compress_coords_into(
            &self.current_coords,
            &mut self.scratch_lat_offsets,
            &mut self.scratch_lon_offsets,
            &mut self.compress_buf,
        );
        let compressed = self.compress_buf.len() < raw_size;

        // Write chunk into flat blob: [node_mask:32][flags_and_len:2][packed_data]
        self.current_group_data
            .extend_from_slice(&self.current_node_mask);
        if compressed {
            let fl = encode_flags_and_len(true, self.compress_buf.len());
            self.current_group_data.extend_from_slice(&fl.to_le_bytes());
            self.current_group_data
                .extend_from_slice(&self.compress_buf);
        } else {
            let fl = encode_flags_and_len(false, raw_size);
            self.current_group_data.extend_from_slice(&fl.to_le_bytes());
            for coord in &self.current_coords {
                self.current_group_data
                    .extend_from_slice(&coord.lat.to_le_bytes());
                self.current_group_data
                    .extend_from_slice(&coord.lon.to_le_bytes());
            }
        }

        self.current_coords.clear();
    }

    fn flush_group(&mut self) {
        if self.current_group_data.is_empty() {
            return;
        }
        // Clone the blob into exact-sized immutable storage, then clear the
        // builder Vec while retaining its capacity for the next group.
        let data = self.current_group_data.clone().into_boxed_slice();
        self.current_group_data.clear();

        let group = Group {
            chunk_mask: self.current_chunk_mask,
            data,
        };
        #[allow(clippy::cast_possible_truncation)]
        let gid = self.current_group_id as usize;
        self.groups[gid] = Some(Box::new(group));
    }

    /// Convert to a read-only reader. Consumes the store.
    #[allow(clippy::cast_possible_truncation, clippy::unwrap_used)]
    fn into_reader(mut self) -> SortedNodeStoreReaderInner {
        self.flush_chunk();
        self.flush_group();

        let node_count = self.node_count;
        let group_count = self.groups.len();

        // An ELIVAGAR_NODE_STATS scan used to live here: it walked every group
        // blob to report chunk counts, compression ratio and blob bytes.
        // Deleted 2026-07-15. It profiled SortedNodeStore, which only the raw
        // path builds - the record path is locations-on-ways, where the enriched PBF
        // deletes the node store entirely. The scan also charged raw-path runs
        // inside phase12_ms. The always-on
        // node_store_nodes / node_store_groups counters carry the ledger need
        // and go to the sidecar, where measurements actually live.

        // A cache key must identify the reader as well as its group/chunk.
        // Allocation addresses can be reused after an earlier reader drops.
        static NEXT_READER_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let cache_id = NEXT_READER_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        SortedNodeStoreReaderInner {
            groups: self.groups,
            node_count,
            group_count,
            cache_id,
        }
    }

    /// Read coordinates (used by tests; not performance-critical).
    /// Read coordinates using a typed node identifier.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn get_id(&self, node_id: NodeId) -> Option<LatLonE7> {
        let id = node_id.as_nonnegative_u64()?;
        let group_id = id / NODES_PER_GROUP;
        let chunk_id = ((id % NODES_PER_GROUP) / NODES_PER_CHUNK as u64) as u8;
        let node_in_chunk = (id % NODES_PER_CHUNK as u64) as u8;

        // Check in-progress group.
        if self.node_count > 0 && group_id == self.current_group_id {
            // Check in-progress chunk first (still uncompressed).
            if chunk_id == self.current_chunk_id {
                if !test_bit(&self.current_node_mask, node_in_chunk) {
                    return None;
                }
                let idx = count_bits_before(&self.current_node_mask, node_in_chunk);
                return Some(self.current_coords[idx]);
            }
            // Check completed chunks in current group's flat blob.
            if test_bit(&self.current_chunk_mask, chunk_id) {
                let chunk_idx = count_bits_before(&self.current_chunk_mask, chunk_id);
                return get_from_blob_chunk(&self.current_group_data, chunk_idx, node_in_chunk);
            }
            return None;
        }

        // Check finalized groups.
        let group = self.groups.get(group_id as usize)?.as_ref()?;
        get_from_group(group, chunk_id, node_in_chunk)
    }

    /// Number of nodes stored.
    fn node_count(&self) -> u64 {
        self.node_count
    }
}

/// Read-only sorted node coordinate store. Safe for concurrent reads
/// from rayon threads (all interior data is immutable).
struct SortedNodeStoreReaderInner {
    groups: Vec<Option<Box<Group>>>,
    node_count: u64,
    group_count: usize,
    cache_id: u64,
}

impl SortedNodeStoreReaderInner {
    fn node_count(&self) -> u64 {
        self.node_count
    }
    fn group_count(&self) -> usize {
        self.group_count
    }

    /// Look up coordinates using a typed node identifier.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn get_id(&self, node_id: NodeId) -> Option<LatLonE7> {
        let id = node_id.as_nonnegative_u64()?;
        let group_id = (id / NODES_PER_GROUP) as usize;
        let chunk_id = ((id % NODES_PER_GROUP) / NODES_PER_CHUNK as u64) as u8;
        let node_in_chunk = (id % NODES_PER_CHUNK as u64) as u8;

        let group = self.groups.get(group_id)?.as_ref()?;
        get_from_group_cached(group, self.cache_id, group_id, chunk_id, node_in_chunk)
    }
}

// ---------------------------------------------------------------------------
// Unified NodeStore / NodeStoreReader API
// ---------------------------------------------------------------------------

#[allow(clippy::large_enum_variant)]
enum NodeStoreBackend {
    Flat(FlatNodeIndexInner),
    Sorted(SortedNodeStoreInner),
}

/// Unified node store for the write phase. Backend types stay private.
pub struct NodeStore {
    backend: NodeStoreBackend,
}

impl NodeStore {
    /// Create a store for node IDs in strictly ascending order.
    pub fn new_sorted() -> Self {
        Self {
            backend: NodeStoreBackend::Sorted(SortedNodeStoreInner::new()),
        }
    }

    /// Create a direct-addressed flat store with its 16 GB safety cap.
    pub fn create_flat(path: &Path) -> io::Result<Self> {
        Ok(Self {
            backend: NodeStoreBackend::Flat(FlatNodeIndexInner::create(path)?),
        })
    }

    /// Compatibility constructor for the flat node-index API.
    pub fn create(path: &Path) -> io::Result<Self> {
        Self::create_flat(path)
    }

    /// Create a direct-addressed flat store without a size safety cap.
    ///
    /// This is intentionally unsafe and should only be used when callers
    /// explicitly opt in to bypass guardrails.
    pub fn create_flat_unbounded(path: &Path) -> io::Result<Self> {
        Ok(Self {
            backend: NodeStoreBackend::Flat(FlatNodeIndexInner::create_unbounded(path)?),
        })
    }

    /// Compatibility constructor for the unbounded flat node-index API.
    pub fn create_unbounded(path: &Path) -> io::Result<Self> {
        Self::create_flat_unbounded(path)
    }

    /// Legacy tuple-coordinate writer.
    pub fn put(&mut self, node_id: i64, lat_e7: i32, lon_e7: i32) -> Result<(), NodeStoreError> {
        self.put_id(NodeId::new(node_id), lat_e7, lon_e7)
    }

    /// Store coordinates using a typed node identifier.
    pub fn put_id(
        &mut self,
        node_id: NodeId,
        lat_e7: i32,
        lon_e7: i32,
    ) -> Result<(), NodeStoreError> {
        self.put_coord_id(node_id, LatLonE7::new(lat_e7, lon_e7))
    }

    /// Store a typed coordinate using a typed node identifier.
    #[inline]
    pub fn put_coord_id(&mut self, node_id: NodeId, coord: LatLonE7) -> Result<(), NodeStoreError> {
        match &mut self.backend {
            NodeStoreBackend::Flat(idx) => idx.put_id(node_id, coord),
            NodeStoreBackend::Sorted(store) => store.put_id(node_id, coord),
        }
    }

    /// Legacy tuple-coordinate lookup while the store is being built.
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        self.get_id(NodeId::new(node_id))
    }

    /// Look up coordinates using a typed node identifier.
    pub fn get_id(&self, node_id: NodeId) -> Option<(i32, i32)> {
        self.get_coord_id(node_id).map(Into::into)
    }

    /// Look up a typed coordinate using a typed node identifier.
    pub fn get_coord_id(&self, node_id: NodeId) -> Option<LatLonE7> {
        match &self.backend {
            NodeStoreBackend::Flat(idx) => idx.get_id(node_id),
            NodeStoreBackend::Sorted(store) => store.get_id(node_id),
        }
    }

    /// Convert to a read-only store after all nodes have been written.
    pub fn into_reader(self) -> io::Result<NodeStoreReader> {
        let backend = match self.backend {
            NodeStoreBackend::Flat(idx) => NodeStoreReaderBackend::Flat(idx.into_reader()?),
            NodeStoreBackend::Sorted(store) => NodeStoreReaderBackend::Sorted(store.into_reader()),
        };
        Ok(NodeStoreReader { backend })
    }
}

/// Statistics available from either node-store backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeStoreStats {
    /// Successful puts. Valid OSM input uses each node ID once.
    pub node_count: u64,
    /// Sorted backend groups; the flat backend has no group hierarchy.
    pub group_count: Option<usize>,
}

enum NodeStoreReaderBackend {
    Flat(FlatNodeIndexReaderInner),
    Sorted(SortedNodeStoreReaderInner),
}

/// Unified read-only node store for parallel lookup.
///
/// The backend enum is matched directly on each lookup. This preserves static
/// dispatch without a per-lookup trait-object call.
pub struct NodeStoreReader {
    backend: NodeStoreReaderBackend,
}

impl NodeStoreReader {
    /// Legacy tuple-coordinate lookup.
    #[inline]
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        self.get_id(NodeId::new(node_id))
    }

    /// Look up coordinates using a typed node identifier.
    #[inline]
    pub fn get_id(&self, node_id: NodeId) -> Option<(i32, i32)> {
        self.get_coord_id(node_id).map(Into::into)
    }

    /// Look up a typed coordinate using a typed node identifier.
    #[inline]
    pub fn get_coord_id(&self, node_id: NodeId) -> Option<LatLonE7> {
        match &self.backend {
            NodeStoreReaderBackend::Flat(reader) => reader.get_id(node_id),
            NodeStoreReaderBackend::Sorted(reader) => reader.get_id(node_id),
        }
    }

    pub fn node_count(&self) -> u64 {
        self.stats().node_count
    }

    pub fn group_count(&self) -> usize {
        self.stats().group_count.unwrap_or(0)
    }

    /// Compatibility statistics for callers that only report sorted stores.
    pub fn sorted_stats(&self) -> Option<(u64, usize)> {
        let stats = self.stats();
        stats.group_count.map(|groups| (stats.node_count, groups))
    }

    /// Return node counts for both backends and group count when available.
    pub fn stats(&self) -> NodeStoreStats {
        match &self.backend {
            NodeStoreReaderBackend::Flat(reader) => NodeStoreStats {
                node_count: reader.node_count,
                group_count: None,
            },
            NodeStoreReaderBackend::Sorted(reader) => NodeStoreStats {
                node_count: reader.node_count(),
                group_count: Some(reader.group_count()),
            },
        }
    }
}

/// Compatibility facade for callers that explicitly build a sorted store.
///
/// Phase12 uses the unified NodeStore, which keeps this backend choice private.
pub struct SortedNodeStore {
    inner: SortedNodeStoreInner,
}

impl Default for SortedNodeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SortedNodeStore {
    pub fn new() -> Self {
        Self {
            inner: SortedNodeStoreInner::new(),
        }
    }

    #[inline]
    pub fn put(&mut self, node_id: i64, lat_e7: i32, lon_e7: i32) -> Result<(), NodeStoreError> {
        self.put_id(NodeId::new(node_id), lat_e7, lon_e7)
    }

    #[inline]
    pub fn put_id(
        &mut self,
        node_id: NodeId,
        lat_e7: i32,
        lon_e7: i32,
    ) -> Result<(), NodeStoreError> {
        self.inner.put_id(node_id, LatLonE7::new(lat_e7, lon_e7))
    }

    #[inline]
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        self.get_id(NodeId::new(node_id))
    }

    #[inline]
    pub fn get_id(&self, node_id: NodeId) -> Option<(i32, i32)> {
        self.inner.get_id(node_id).map(Into::into)
    }

    pub fn node_count(&self) -> u64 {
        self.inner.node_count()
    }

    pub fn into_reader(self) -> SortedNodeStoreReader {
        NodeStoreReader {
            backend: NodeStoreReaderBackend::Sorted(self.inner.into_reader()),
        }
    }
}

/// Compatibility names now resolve to the unified writer and reader.
pub type NodeIndex = NodeStore;
pub type NodeIndexReader = NodeStoreReader;
pub type SortedNodeStoreReader = NodeStoreReader;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn create_and_get_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let idx = NodeIndex::create(&path).unwrap();
        assert_eq!(idx.get(1), None);
    }

    #[test]
    fn put_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(100, 555_000_000, 133_000_000).unwrap();
        assert_eq!(idx.get(100), Some((555_000_000, 133_000_000)));
    }

    #[test]
    fn put_multiple_get_each() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(10, 100_000, 200_000).unwrap();
        idx.put(20, 300_000, 400_000).unwrap();
        idx.put(30, 500_000, 600_000).unwrap();
        assert_eq!(idx.get(10), Some((100_000, 200_000)));
        assert_eq!(idx.get(20), Some((300_000, 400_000)));
        assert_eq!(idx.get(30), Some((500_000, 600_000)));
    }

    #[test]
    fn get_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(5, 111, 222).unwrap();
        assert_eq!(idx.get(999), None);
    }

    #[test]
    fn zero_zero_is_valid() {
        // Nodes at lat=0, lon=0 (Gulf of Guinea) should be stored and retrieved correctly.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(1, 0, 0).unwrap();
        assert_eq!(idx.get(1), Some((0, 0)));
    }

    #[test]
    fn overwrite_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(42, 111_000, 222_000).unwrap();
        idx.put(42, 333_000, 444_000).unwrap();
        assert_eq!(idx.get(42), Some((333_000, 444_000)));
    }

    #[test]
    fn into_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(10, 100_000, 200_000).unwrap();
        idx.put(20, 300_000, 400_000).unwrap();

        let reader = idx.into_reader().unwrap();
        assert_eq!(reader.get(10), Some((100_000, 200_000)));
        assert_eq!(reader.get(20), Some((300_000, 400_000)));
        assert_eq!(reader.get(999), None);
    }

    // --- Bitmask helper tests ---

    #[test]
    fn bitmask_set_and_test() {
        let mut mask = [0u8; BITMASK_BYTES];
        assert!(!test_bit(&mask, 0));
        set_bit(&mut mask, 0);
        assert!(test_bit(&mask, 0));
        assert!(!test_bit(&mask, 1));

        set_bit(&mut mask, 255);
        assert!(test_bit(&mask, 255));
        assert!(!test_bit(&mask, 254));
    }

    #[test]
    fn bitmask_count_before() {
        let mut mask = [0u8; BITMASK_BYTES];
        set_bit(&mut mask, 3);
        set_bit(&mut mask, 7);
        set_bit(&mut mask, 10);
        assert_eq!(count_bits_before(&mask, 3), 0);
        assert_eq!(count_bits_before(&mask, 7), 1);
        assert_eq!(count_bits_before(&mask, 10), 2);
        assert_eq!(count_bits_before(&mask, 200), 3);
    }

    #[test]
    fn bitmask_count_before_dense() {
        let mask = [0xFF_u8; BITMASK_BYTES];
        assert_eq!(count_bits_before(&mask, 0), 0);
        assert_eq!(count_bits_before(&mask, 128), 128);
        assert_eq!(count_bits_before(&mask, 255), 255);
    }

    // --- FOR codec property tests ---

    #[test]
    fn for_codec_round_trips_full_i32_range_at_every_chunk_length() {
        fn next_i32(state: &mut u64) -> i32 {
            *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut value = *state;
            value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            let bytes = (value ^ (value >> 31)).to_le_bytes();
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        }

        let mut state = 0x7A31_19D4_C2E5_806B;
        for n in 1..=NODES_PER_CHUNK {
            let mut lats = (0..n).map(|_| next_i32(&mut state)).collect::<Vec<_>>();
            let mut lons = (0..n).map(|_| next_i32(&mut state)).collect::<Vec<_>>();
            if n >= 2 {
                lats[0] = i32::MIN;
                lats[n - 1] = i32::MAX;
                lons[0] = i32::MAX;
                lons[n - 1] = i32::MIN;
            }
            let coords = lats
                .iter()
                .copied()
                .zip(lons.iter().copied())
                .map(LatLonE7::from)
                .collect::<Vec<_>>();

            let mut lat_offsets = Vec::with_capacity(n);
            let mut lon_offsets = Vec::with_capacity(n);
            let mut packed = Vec::new();
            compress_coords_into(&coords, &mut lat_offsets, &mut lon_offsets, &mut packed);

            if n >= 2 {
                assert_eq!(packed[8], 32, "latitude width at chunk length {n}");
                assert_eq!(packed[9], 32, "longitude width at chunk length {n}");
            }

            let mut decoded = vec![LatLonE7::default(); n];
            decompress_chunk(n, true, &packed, &mut decoded);
            assert_eq!(decoded, coords, "round trip at chunk length {n}");
        }
    }

    // --- SortedNodeStore tests ---

    #[test]
    fn sorted_create_and_get_empty() {
        let store = SortedNodeStore::new();
        assert_eq!(store.get(1), None);
    }

    #[test]
    fn sorted_put_and_get() {
        let mut store = SortedNodeStore::new();
        store.put(100, 555_000_000, 133_000_000).unwrap();
        assert_eq!(store.get(100), Some((555_000_000, 133_000_000)));
    }

    #[test]
    fn sorted_put_multiple_get_each() {
        let mut store = SortedNodeStore::new();
        store.put(10, 100_000, 200_000).unwrap();
        store.put(20, 300_000, 400_000).unwrap();
        store.put(30, 500_000, 600_000).unwrap();
        assert_eq!(store.get(10), Some((100_000, 200_000)));
        assert_eq!(store.get(20), Some((300_000, 400_000)));
        assert_eq!(store.get(30), Some((500_000, 600_000)));
    }

    #[test]
    fn sorted_get_nonexistent() {
        let mut store = SortedNodeStore::new();
        store.put(5, 111, 222).unwrap();
        assert_eq!(store.get(999), None);
    }

    #[test]
    fn sorted_zero_zero_is_valid() {
        let mut store = SortedNodeStore::new();
        store.put(1, 0, 0).unwrap();
        assert_eq!(store.get(1), Some((0, 0)));
    }

    #[test]
    fn sorted_into_reader() {
        let mut store = SortedNodeStore::new();
        store.put(10, 100_000, 200_000).unwrap();
        store.put(20, 300_000, 400_000).unwrap();
        let reader = store.into_reader();
        assert_eq!(reader.get(10), Some((100_000, 200_000)));
        assert_eq!(reader.get(20), Some((300_000, 400_000)));
        assert_eq!(reader.get(999), None);
    }

    #[test]
    fn sorted_compressed_chunk_spans_distant_longitudes() {
        let mut store = SortedNodeStore::new();
        // Repeated latitudes keep the chunk compressed even with 32-bit
        // longitude offsets. Their span exceeds i32::MAX in E7 units.
        for id in 1..=32 {
            let lon = if id % 2 == 0 {
                1_800_000_000
            } else {
                -1_800_000_000
            };
            store.put(id, 100, lon).unwrap();
        }
        store.put(256, 200, 300).unwrap(); // flush the first chunk for builder reads
        for id in 1..=32 {
            let lon = if id % 2 == 0 {
                1_800_000_000
            } else {
                -1_800_000_000
            };
            assert_eq!(store.get(id), Some((100, lon)));
        }
        let reader = store.into_reader();
        for id in 1..=32 {
            let lon = if id % 2 == 0 {
                1_800_000_000
            } else {
                -1_800_000_000
            };
            assert_eq!(reader.get(id), Some((100, lon)));
        }
    }

    #[test]
    fn sorted_reader_caches_do_not_mix_stores() {
        let mut first = SortedNodeStore::new();
        first.put(10, 100, 200).unwrap();
        let first = first.into_reader();
        let mut second = SortedNodeStore::new();
        second.put(10, 300, 400).unwrap();
        second.put(20, 500, 600).unwrap();
        let second = second.into_reader();

        assert_eq!(first.get(10), Some((100, 200)));
        assert_eq!(second.get(10), Some((300, 400)));
        assert_eq!(second.get(20), Some((500, 600)));
        assert_eq!(first.get(20), None);
        assert_eq!(first.get(10), Some((100, 200)));
    }

    #[test]
    fn sorted_cross_chunk_boundary() {
        let mut store = SortedNodeStore::new();
        store.put(255, 10, 20).unwrap();
        store.put(256, 30, 40).unwrap();
        assert_eq!(store.get(255), Some((10, 20)));
        assert_eq!(store.get(256), Some((30, 40)));
    }

    #[test]
    fn sorted_cross_group_boundary() {
        let mut store = SortedNodeStore::new();
        store.put(65535, 10, 20).unwrap();
        store.put(65536, 30, 40).unwrap();
        let reader = store.into_reader();
        assert_eq!(reader.get(65535), Some((10, 20)));
        assert_eq!(reader.get(65536), Some((30, 40)));
    }

    #[test]
    fn sorted_large_gap_in_ids() {
        let mut store = SortedNodeStore::new();
        store.put(1_000_000, 10, 20).unwrap();
        store.put(10_000_000, 30, 40).unwrap();
        let reader = store.into_reader();
        assert_eq!(reader.get(1_000_000), Some((10, 20)));
        assert_eq!(reader.get(10_000_000), Some((30, 40)));
        assert_eq!(reader.get(5_000_000), None);
    }

    #[test]
    fn sorted_single_node() {
        let mut store = SortedNodeStore::new();
        store.put(42, 100, 200).unwrap();
        let reader = store.into_reader();
        assert_eq!(reader.get(42), Some((100, 200)));
        assert_eq!(reader.get(41), None);
        assert_eq!(reader.get(43), None);
    }

    #[test]
    fn sorted_dense_chunk() {
        let mut store = SortedNodeStore::new();
        for i in 0..256i64 {
            #[allow(clippy::cast_possible_truncation)]
            store.put(i, i as i32 * 100, i as i32 * 200).unwrap();
        }
        let reader = store.into_reader();
        for i in 0..256i64 {
            #[allow(clippy::cast_possible_truncation)]
            {
                assert_eq!(reader.get(i), Some((i as i32 * 100, i as i32 * 200)));
            }
        }
    }

    #[test]
    fn sorted_empty_into_reader() {
        let store = SortedNodeStore::new();
        let reader = store.into_reader();
        assert_eq!(reader.get(0), None);
        assert_eq!(reader.get(1_000_000), None);
    }

    #[test]
    fn sorted_high_node_id() {
        let mut store = SortedNodeStore::new();
        store.put(12_000_000_000, 550_000_000, 130_000_000).unwrap();
        let reader = store.into_reader();
        assert_eq!(reader.get(12_000_000_000), Some((550_000_000, 130_000_000)));
    }

    #[test]
    fn sorted_refuses_ids_past_its_dense_directory_range() {
        let mut store = SortedNodeStore::new();
        let limit = i64::try_from(SORTED_NODE_ID_LIMIT).unwrap();
        let err = store.put(limit, 1, 1).unwrap_err();
        assert!(matches!(err, NodeStoreError::IdCap { node_id, .. } if node_id == limit));
        assert!(err.is_input_rejection());
        // The refusal left no state behind: the ID below the limit stores.
        store.put(limit - 1, 1, 1).unwrap();
        assert_eq!(store.into_reader().get(limit - 1), Some((1, 1)));
    }

    #[test]
    fn sorted_rejects_non_monotonic() {
        let mut store = SortedNodeStore::new();
        store.put(10, 100, 200).unwrap();
        let err = store.put(5, 300, 400).unwrap_err();
        assert!(matches!(
            err,
            NodeStoreError::NotIncreasing {
                node_id: 5,
                previous: 10
            }
        ));
        assert!(err.is_input_rejection());
    }

    #[test]
    fn sorted_rejects_duplicate_id() {
        let mut store = SortedNodeStore::new();
        store.put(10, 100, 200).unwrap();
        let err = store.put(10, 300, 400).unwrap_err();
        assert!(matches!(
            err,
            NodeStoreError::NotIncreasing { node_id: 10, .. }
        ));
    }

    #[test]
    fn both_backends_reject_negative_ids() {
        let mut sorted = NodeStore::new_sorted();
        assert!(matches!(
            sorted.put(-1, 0, 0),
            Err(NodeStoreError::NegativeId { node_id: -1 })
        ));
        let dir = tempfile::tempdir().unwrap();
        let mut flat = NodeStore::create_flat(&dir.path().join("neg.bin")).unwrap();
        assert!(matches!(
            flat.put(-1, 0, 0),
            Err(NodeStoreError::NegativeId { node_id: -1 })
        ));
    }

    #[test]
    fn unbounded_flat_rejects_unaddressable_ids() {
        let dir = tempfile::tempdir().unwrap();
        let mut flat = NodeStore::create_flat_unbounded(&dir.path().join("big.bin")).unwrap();
        let err = flat.put(i64::MAX, 1, 1).unwrap_err();
        assert!(matches!(
            err,
            NodeStoreError::OffsetOverflow { node_id: i64::MAX }
        ));
        assert!(err.is_input_rejection());
    }

    #[test]
    fn grow_failures_keep_their_io_cause() {
        let err = NodeStoreError::Grow(io::Error::from_raw_os_error(28));
        assert!(!err.is_input_rejection());
        let source = std::error::Error::source(&err)
            .and_then(|e| e.downcast_ref::<io::Error>())
            .unwrap();
        assert_eq!(source.raw_os_error(), Some(28));
    }

    #[test]
    fn node_store_sorted_variant() {
        let mut store = NodeStore::new_sorted();
        store
            .put_coord_id(NodeId::new(100), LatLonE7::new(555_000_000, 133_000_000))
            .unwrap();
        let reader = store.into_reader().unwrap();
        assert_eq!(
            reader.get_coord_id(NodeId::new(100)),
            Some(LatLonE7::new(555_000_000, 133_000_000))
        );
        assert_eq!(reader.get(999), None);
    }

    #[test]
    fn node_store_flat_variant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut store = NodeStore::create_flat(&path).unwrap();
        store.put(100, 555_000_000, 133_000_000).unwrap();
        let reader = store.into_reader().unwrap();
        assert_eq!(reader.get(100), Some((555_000_000, 133_000_000)));
        assert_eq!(reader.get(999), None);
    }

    #[test]
    fn flat_index_safety_cap_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_cap.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        #[allow(clippy::cast_possible_wrap)]
        let node_id = ((MAX_FLAT_INDEX_SIZE / ENTRY_SIZE) + 1) as i64;
        let err = idx.put(node_id, 1, 1).unwrap_err();
        assert!(matches!(
            err,
            NodeStoreError::SafetyCap { node_id: id, cap_bytes: MAX_FLAT_INDEX_SIZE, .. }
                if id == node_id
        ));
        assert!(err.is_input_rejection());
        assert!(
            err.to_string()
                .contains("flat node index safety cap exceeded")
        );
    }
}
