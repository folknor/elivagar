// Node coordinate index — two implementations.
//
// 1. NodeIndex (flat mmap): Direct-addressed at `node_id * 8`, 4 bytes lat_e7 +
//    4 bytes lon_e7. Grows in 1 GB increments, backed by mmap. Works for any
//    insertion order. Planet scale: ~96 GB file (IDs up to ~12B).
//
// 2. SortedNodeStore (compact in-RAM): Requires sorted insertion (ascending
//    node IDs, guaranteed by PBF Sort.Type_then_ID). Three-level hierarchy:
//    groups (64K nodes) → chunks (256 nodes) → individual nodes. Each level
//    uses a 256-bit bitmask for O(1) presence test + popcount-based indexing
//    into packed arrays. Denmark: ~420 MB vs 96 GB sparse mmap.
//
// NodeStore / NodeStoreReader enums dispatch to the appropriate backend.
//
// Flat mmap history: all madvise hints were tried and removed. See CLAUDE.md.

use std::cell::UnsafeCell;
use std::fs::File;
use std::io;
use std::path::Path;

use memmap2::{Mmap, MmapMut};

const ENTRY_SIZE: u64 = 8; // 4 bytes lat_e7 + 4 bytes lon_e7
const GROW_INCREMENT: u64 = 1_073_741_824; // 1 GB

// XOR mask applied to stored coordinates so that (0,0) on disk means "unset"
// while a real node at lat=0, lon=0 stores as non-zero.
// 0x55555555 = 1431655765 E7 = 143.17° — outside valid latitude range [-90°, 90°],
// so no real coordinate pair can XOR to (0, 0).
#[allow(clippy::cast_possible_wrap)]
const COORD_XOR: i32 = 0x5555_5555_u32 as i32;

pub struct NodeIndex {
    file: File,
    mmap: MmapMut,
    file_len: u64,
}

impl NodeIndex {
    /// Create a new writable node index file at `path`.
    ///
    /// No madvise hints are set during the write phase — see module-level
    /// comment for history on why MADV_SEQUENTIAL was removed.
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        let file_len = GROW_INCREMENT;
        file.set_len(file_len)?;

        let mmap = unsafe { MmapMut::map_mut(&file)? };

        Ok(NodeIndex {
            file,
            mmap,
            file_len,
        })
    }

    /// Write coordinates for a node. Grows the file if needed.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn put(&mut self, node_id: i64, lat_e7: i32, lon_e7: i32) {
        let offset = node_id as u64 * ENTRY_SIZE;
        let needed = offset + ENTRY_SIZE;

        if needed > self.file_len {
            // Grow in 1 GB increments until large enough.
            let mut new_len = self.file_len;
            while new_len < needed {
                new_len += GROW_INCREMENT;
            }
            // Panic: unrecoverable I/O — disk full or mmap failure means the run is dead.
            self.file.set_len(new_len).expect("failed to grow node index file");
            self.mmap = unsafe { MmapMut::map_mut(&self.file).expect("failed to remap node index") };

            self.file_len = new_len;
        }

        let off = offset as usize;
        self.mmap[off..off + 4].copy_from_slice(&(lat_e7 ^ COORD_XOR).to_le_bytes());
        self.mmap[off + 4..off + 8].copy_from_slice(&(lon_e7 ^ COORD_XOR).to_le_bytes());
    }

    /// Read coordinates for a node. Returns None if entry is unset (all zeros).
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation, clippy::unwrap_used)]
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        get_from_mmap(&self.mmap, self.file_len, node_id)
    }

    /// Convert to a read-only reader after all nodes have been written.
    /// Consumes the writable index — no more `put()` calls are possible.
    ///
    /// The returned `NodeIndexReader` wraps a read-only `Mmap` which is `Sync`,
    /// allowing safe concurrent reads from rayon worker threads.
    pub fn into_reader(self) -> io::Result<NodeIndexReader> {
        let file_len = self.file_len;
        let mmap = self.mmap.make_read_only()?;
        Ok(NodeIndexReader { mmap, file_len })
    }
}

/// Shared get logic for both NodeIndex and NodeIndexReader.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation, clippy::unwrap_used)]
fn get_from_mmap(mmap: &[u8], file_len: u64, node_id: i64) -> Option<(i32, i32)> {
    let offset = node_id as u64 * ENTRY_SIZE;
    let needed = offset + ENTRY_SIZE;

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
        Some((lat_raw ^ COORD_XOR, lon_raw ^ COORD_XOR))
    }
}

/// Read-only node coordinate index. Safe for concurrent reads from rayon threads
/// (`Mmap` is `Sync + Send`).
///
/// Created from `NodeIndex::into_reader()` after all nodes have been written.
pub struct NodeIndexReader {
    mmap: Mmap,
    file_len: u64,
}

impl NodeIndexReader {
    /// Read coordinates for a node. Returns None if entry is unset (all zeros).
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        get_from_mmap(&self.mmap, self.file_len, node_id)
    }
}

// ---------------------------------------------------------------------------
// SortedNodeStore — compact hierarchical store for sorted PBF files
// ---------------------------------------------------------------------------

// Tried NODES_PER_CHUNK = 512 (halving chunk count to reduce decompress_chunk calls).
// Result: 6.5% fewer decompress calls but each 2x slower (decompressing 512 vs 256 nodes),
// blob grew 270→294 MB increasing DRAM pressure. Net PBF phase +800ms regression. Reverted.
const NODES_PER_CHUNK: usize = 256;
const NODES_PER_GROUP: u64 = 256 * 256; // 65536
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
    let full: usize = mask[..byte_idx].iter().map(|b| b.count_ones() as usize).sum();
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
// Exact-size bitpacking — pack/unpack N values at a given bit-width.
// Replaces BitPacker4x (128-value blocks with padding) so every chunk
// compresses at its actual size.
// ---------------------------------------------------------------------------

/// Pack `values` at `bit_width` bits each, appending to `dest`.
/// Output size: ⌈values.len() × bit_width / 8⌉ bytes.
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
/// Doesn't matter anyway — decompress_chunk is DRAM-latency-bound on real data
/// (820ns avg on 270 MB blob vs 25ns synthetic with L1-hot data).
#[allow(clippy::cast_possible_truncation, clippy::explicit_iter_loop, clippy::unwrap_used)]
fn bitunpack_values(packed: &[u8], n: usize, bit_width: u8, out: &mut [u32]) {
    if bit_width == 0 {
        for o in out[..n].iter_mut() {
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
        // when ≥8 bytes remain. Slow path: byte-by-byte near end of buffer.
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
    { (32 - max_val.leading_zeros()) as u8 }
}

/// Per-group flat blob layout. Each chunk is stored inline as:
/// `[node_mask: 32 bytes][flags_and_len: u16][packed_data: len bytes]`
/// where flags_and_len bit 15 = compressed, bits 0-14 = packed_data length.
/// No separate struct per chunk — everything lives in `data`.
const CHUNK_HEADER_SIZE: usize = BITMASK_BYTES + 2; // 34 bytes

/// Encode flags_and_len: bit 15 = compressed, bits 0-14 = packed_len.
#[inline]
#[allow(clippy::cast_possible_truncation)]
fn encode_flags_and_len(compressed: bool, packed_len: usize) -> u16 {
    debug_assert!(packed_len <= 0x7FFF, "packed_len {packed_len} exceeds 15-bit limit");
    let flags = if compressed { 0x8000u16 } else { 0u16 };
    flags | packed_len as u16
}

struct Group {
    chunk_mask: [u8; BITMASK_BYTES],
    data: Box<[u8]>, // flat blob: all chunks' headers + packed data
}

/// Compress coordinates with exact-size FOR and append to `dest`.
/// Format: [lat_min:4][lon_min:4][lat_bits:1][lon_bits:1][packed lat][packed lon]
/// Packs exactly N values — no padding to block boundaries.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
fn compress_coords_into(
    lats: &[i32],
    lons: &[i32],
    lat_offsets: &mut Vec<u32>,
    lon_offsets: &mut Vec<u32>,
    dest: &mut Vec<u8>,
) {
    let n = lats.len();

    // Compute min values and offsets
    let lat_min = lats.iter().copied().min().unwrap_or(0);
    let lon_min = lons.iter().copied().min().unwrap_or(0);

    #[allow(clippy::cast_sign_loss)]
    let lat_max_offset = lats.iter().map(|&v| (v - lat_min) as u32).max().unwrap_or(0);
    #[allow(clippy::cast_sign_loss)]
    let lon_max_offset = lons.iter().map(|&v| (v - lon_min) as u32).max().unwrap_or(0);

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
    for i in 0..n {
        lat_offsets.push((lats[i] - lat_min) as u32);
        lon_offsets.push((lons[i] - lon_min) as u32);
    }

    bitpack_values_into(lat_offsets, lat_bits, dest);
    bitpack_values_into(lon_offsets, lon_bits, dest);
}

/// Decompress all coordinates from a chunk into the output slice.
/// `node_mask` and `packed` come from the flat blob; `compressed` from flags_and_len.
/// For compressed: [lat_min:4][lon_min:4][lat_bits:1][lon_bits:1][packed lat][packed lon]
/// For raw: sequential (i32, i32) pairs.
///
/// This is the dominant cost in the PBF read phase. On Denmark (dm6):
///   ~24M calls, 820ns avg, ~20s cumulative (with 4-entry LRU cache).
///   The 270 MB blob doesn't fit in L2 cache, so each miss fetches from DRAM.
///   Synthetic benchmarks show 25ns/call (L1-hot) — NOT representative.
///   Compute optimizations (accumulator unpacking, etc.) don't help because
///   the CPU is waiting on memory, not on arithmetic.
#[hotpath::measure]
#[allow(clippy::unwrap_used)]
fn decompress_chunk(
    node_mask: &[u8; BITMASK_BYTES],
    compressed: bool,
    packed: &[u8],
    out: &mut [(i32, i32)],
) {
    let n = count_set_bits(node_mask) as usize;

    if !compressed {
        // Raw (i32, i32) pairs — no FOR encoding
        for i in 0..n {
            let off = i * 8;
            let lat = i32::from_le_bytes(packed[off..off + 4].try_into().unwrap());
            let lon = i32::from_le_bytes(packed[off + 4..off + 8].try_into().unwrap());
            out[i] = (lat, lon);
        }
        return;
    }

    // Read 10-byte header
    let lat_min = i32::from_le_bytes(packed[0..4].try_into().unwrap());
    let lon_min = i32::from_le_bytes(packed[4..8].try_into().unwrap());
    let lat_bits = packed[8];
    let lon_bits = packed[9];

    // Unpack lat offsets
    let lat_packed_bytes = (n * lat_bits as usize + 7) / 8;
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
            out[i] = (
                lat_min + lat_buf[i] as i32,
                lon_min + lon_buf[i] as i32,
            );
        }
    }
}

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
/// (pre-computed byte offsets per chunk) to make this O(1) — reverted because
/// cache hit rate is ~76%, so this function only runs on ~24% of lookups,
/// and Denmark averages ~7 chunks/group so the scan is short anyway.
#[hotpath::measure]
#[allow(clippy::unwrap_used)]
fn find_chunk_in_blob(data: &[u8], chunk_idx: usize) -> ChunkRef<'_> {
    let mut offset = 0;
    for _ in 0..chunk_idx {
        // Skip: node_mask (32) + flags_and_len (2) + packed data
        let fl = u16::from_le_bytes([data[offset + BITMASK_BYTES], data[offset + BITMASK_BYTES + 1]]);
        let packed_len = (fl & 0x7FFF) as usize;
        offset += CHUNK_HEADER_SIZE + packed_len;
    }
    let node_mask: &[u8; BITMASK_BYTES] = data[offset..offset + BITMASK_BYTES]
        .try_into()
        .unwrap();
    let fl = u16::from_le_bytes([data[offset + BITMASK_BYTES], data[offset + BITMASK_BYTES + 1]]);
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
    group_id: usize,
    chunk_idx: usize,
    node_mask: [u8; BITMASK_BYTES],
    coords: [(i32, i32); NODES_PER_CHUNK],
    count: u16, // 0 = empty
}

impl CacheEntry {
    fn new() -> Self {
        CacheEntry {
            group_id: usize::MAX,
            chunk_idx: usize::MAX,
            node_mask: [0u8; BITMASK_BYTES],
            coords: [(0, 0); NODES_PER_CHUNK],
            count: 0,
        }
    }
}

/// Thread-local decompression cache with LRU eviction.
///
/// Started with 1 entry — ways that span 2 nearby chunks caused constant
/// eviction (ping-ponging). 4 entries cut decompress_chunk calls by 17%
/// (28.7M→23.8M on Denmark) and avg latency by 42% (1.41µs→820ns).
/// Total decompress_chunk time: 40.4s→19.5s (-52%). PBF phase -800ms.
///
/// LRU policy: on hit, swap entry to front; on miss, evict last entry.
/// `entries.swap()` avoids memcpy of the large coords arrays.
const CACHE_ENTRIES: usize = 4;

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
            ],
        }
    }
}

// UnsafeCell instead of RefCell: eliminates runtime borrow-check overhead on every
// cache access (~48M calls on Denmark). Measured -3.1% on synthetic way-like lookups.
// SAFETY: thread_local! guarantees single-threaded access — no concurrent borrows.
thread_local! {
    static DECOMPRESS_CACHE: UnsafeCell<DecompressCache> = UnsafeCell::new(DecompressCache::new());
}

/// Look up a node within a finalized Group, using the thread-local cache.
/// On cache hit, skips the blob scan entirely — uses cached node_mask.
///
/// No #[hotpath::measure] — at ~48M calls on Denmark, the two clock_gettime
/// syscalls per call added >50% overhead. Removed in 178ca73.
#[inline]
fn get_from_group_cached(
    group: &Group,
    group_id: usize,
    chunk_id: u8,
    node_in_chunk: u8,
) -> Option<(i32, i32)> {
    if !test_bit(&group.chunk_mask, chunk_id) {
        return None;
    }
    let chunk_idx = count_bits_before(&group.chunk_mask, chunk_id);

    // SAFETY: thread_local! guarantees single-threaded access — no concurrent borrows possible.
    DECOMPRESS_CACHE.with(|cell| {
        let cache = unsafe { &mut *cell.get() };

        // Search cache entries for a hit
        for i in 0..CACHE_ENTRIES {
            let entry = &cache.entries[i];
            if entry.group_id == group_id && entry.chunk_idx == chunk_idx && entry.count != 0 {
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

        // Cache miss — scan blob, decompress, evict LRU (last) entry
        let chunk = find_chunk_in_blob(&group.data, chunk_idx);
        if !test_bit(chunk.node_mask, node_in_chunk) {
            return None;
        }
        let node_idx = count_bits_before(chunk.node_mask, node_in_chunk);

        // Evict last entry: swap it to front, then overwrite
        cache.entries.swap(0, CACHE_ENTRIES - 1);
        let entry = &mut cache.entries[0];
        decompress_chunk(chunk.node_mask, chunk.compressed, chunk.packed, &mut entry.coords);
        entry.node_mask = *chunk.node_mask;
        entry.group_id = group_id;
        entry.chunk_idx = chunk_idx;
        entry.count = count_set_bits(chunk.node_mask);
        Some(entry.coords[node_idx])
    })
}

/// Look up a single node from a chunk in a blob (no cache, for builder/test path).
#[inline]
fn get_from_blob_chunk(data: &[u8], chunk_idx: usize, node_in_chunk: u8) -> Option<(i32, i32)> {
    let chunk = find_chunk_in_blob(data, chunk_idx);
    if !test_bit(chunk.node_mask, node_in_chunk) {
        return None;
    }
    let node_idx = count_bits_before(chunk.node_mask, node_in_chunk);
    let mut coords = [(0i32, 0i32); NODES_PER_CHUNK];
    decompress_chunk(chunk.node_mask, chunk.compressed, chunk.packed, &mut coords);
    Some(coords[node_idx])
}

/// Look up a node within a finalized Group (no cache, for builder/test path).
#[inline]
fn get_from_group(group: &Group, chunk_id: u8, node_in_chunk: u8) -> Option<(i32, i32)> {
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
pub struct SortedNodeStore {
    /// Completed groups, indexed by group_id. None = no nodes in that group.
    groups: Vec<Option<Box<Group>>>,

    // Builder state for the group being accumulated.
    current_group_id: u64,
    current_chunk_mask: [u8; BITMASK_BYTES],
    current_group_data: Vec<u8>,  // flat blob being built for current group
    compress_buf: Vec<u8>,        // scratch buffer for compression, reused
    scratch_lats: Vec<i32>,       // scratch buffer for flush_chunk, reused
    scratch_lons: Vec<i32>,       // scratch buffer for flush_chunk, reused
    scratch_lat_offsets: Vec<u32>, // scratch buffer for compress_coords_into, reused
    scratch_lon_offsets: Vec<u32>, // scratch buffer for compress_coords_into, reused

    // Builder state for the chunk being accumulated.
    current_chunk_id: u8,
    current_node_mask: [u8; BITMASK_BYTES],
    current_coords: Vec<(i32, i32)>,

    last_node_id: i64,
    node_count: u64,
}

impl SortedNodeStore {
    pub fn new() -> Self {
        SortedNodeStore {
            groups: Vec::new(),
            current_group_id: 0,
            current_chunk_mask: [0u8; BITMASK_BYTES],
            current_group_data: Vec::new(),
            compress_buf: Vec::new(),
            scratch_lats: Vec::with_capacity(NODES_PER_CHUNK),
            scratch_lons: Vec::with_capacity(NODES_PER_CHUNK),
            scratch_lat_offsets: Vec::with_capacity(NODES_PER_CHUNK),
            scratch_lon_offsets: Vec::with_capacity(NODES_PER_CHUNK),
            current_chunk_id: 0,
            current_node_mask: [0u8; BITMASK_BYTES],
            current_coords: Vec::with_capacity(NODES_PER_CHUNK),
            last_node_id: -1,
            node_count: 0,
        }
    }

    /// Store coordinates for a node. Node IDs MUST be strictly increasing.
    #[hotpath::measure]
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn put(&mut self, node_id: i64, lat_e7: i32, lon_e7: i32) {
        assert!(
            node_id > self.last_node_id,
            "SortedNodeStore: node IDs must be strictly increasing, got {node_id} after {}",
            self.last_node_id
        );
        self.last_node_id = node_id;
        self.node_count += 1;

        let id = node_id as u64;
        let group_id = id / NODES_PER_GROUP;
        let chunk_id = ((id % NODES_PER_GROUP) / NODES_PER_CHUNK as u64) as u8;
        let node_in_chunk = (id % NODES_PER_CHUNK as u64) as u8;

        if self.node_count == 1 {
            // First node: initialize builder state.
            if self.groups.len() <= group_id as usize {
                self.groups.resize_with(group_id as usize + 1, || None);
            }
            self.current_group_id = group_id;
            self.current_chunk_id = chunk_id;
        } else if group_id != self.current_group_id {
            // New group: flush current chunk + group.
            self.flush_chunk();
            self.flush_group();
            if self.groups.len() <= group_id as usize {
                self.groups.resize_with(group_id as usize + 1, || None);
            }
            self.current_group_id = group_id;
            self.current_chunk_mask = [0u8; BITMASK_BYTES];
            // current_group_data already cleared by flush_group
            self.current_chunk_id = chunk_id;
            self.current_node_mask = [0u8; BITMASK_BYTES];
            self.current_coords = Vec::with_capacity(NODES_PER_CHUNK);
        } else if chunk_id != self.current_chunk_id {
            // Same group, new chunk: flush current chunk.
            self.flush_chunk();
            self.current_chunk_id = chunk_id;
            self.current_node_mask = [0u8; BITMASK_BYTES];
            self.current_coords = Vec::with_capacity(NODES_PER_CHUNK);
        }

        set_bit(&mut self.current_node_mask, node_in_chunk);
        self.current_coords.push((lat_e7, lon_e7));
    }

    // scratch_lats, scratch_lons, scratch_lat_offsets, scratch_lon_offsets are hoisted
    // onto the struct to avoid alloc/dealloc per call (~205K calls on Denmark).
    // Hoisting gave -5.3% build time, -6.8% way-like lookup time (9e014d1).
    #[hotpath::measure]
    fn flush_chunk(&mut self) {
        if self.current_coords.is_empty() {
            return;
        }
        set_bit(&mut self.current_chunk_mask, self.current_chunk_id);

        let n = self.current_coords.len();
        let raw_size = n * 8; // 4 bytes lat + 4 bytes lon

        self.scratch_lats.clear();
        self.scratch_lons.clear();
        for &(lat, lon) in &self.current_coords {
            self.scratch_lats.push(lat);
            self.scratch_lons.push(lon);
        }

        // Compress into scratch buffer
        self.compress_buf.clear();
        compress_coords_into(
            &self.scratch_lats,
            &self.scratch_lons,
            &mut self.scratch_lat_offsets,
            &mut self.scratch_lon_offsets,
            &mut self.compress_buf,
        );
        let compressed = self.compress_buf.len() < raw_size;

        // Write chunk into flat blob: [node_mask:32][flags_and_len:2][packed_data]
        self.current_group_data.extend_from_slice(&self.current_node_mask);
        if compressed {
            let fl = encode_flags_and_len(true, self.compress_buf.len());
            self.current_group_data.extend_from_slice(&fl.to_le_bytes());
            self.current_group_data.extend_from_slice(&self.compress_buf);
        } else {
            let fl = encode_flags_and_len(false, raw_size);
            self.current_group_data.extend_from_slice(&fl.to_le_bytes());
            for &(lat, lon) in &self.current_coords {
                self.current_group_data.extend_from_slice(&lat.to_le_bytes());
                self.current_group_data.extend_from_slice(&lon.to_le_bytes());
            }
        }

        self.current_coords.clear();
    }

    fn flush_group(&mut self) {
        if self.current_group_data.is_empty() {
            return;
        }
        // Clone blob into an exact-sized Box, then clear the Vec to reuse
        // its allocation for the next group. This avoids repeated alloc/dealloc
        // cycles that cause mimalloc fragmentation.
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
    pub fn into_reader(mut self) -> SortedNodeStoreReader {
        self.flush_chunk();
        self.flush_group();

        // Diagnostic: measure actual node store memory usage
        let mut total_blob_bytes: usize = 0;
        let mut total_chunks: usize = 0;
        let mut groups_used: usize = 0;
        let mut uncompressed_chunks: usize = 0;
        for group in &self.groups {
            if let Some(g) = group {
                groups_used += 1;
                total_blob_bytes += g.data.len();
                // Count chunks by scanning the blob
                let mut offset = 0;
                while offset < g.data.len() {
                    total_chunks += 1;
                    let fl = u16::from_le_bytes(
                        g.data[offset + BITMASK_BYTES..offset + BITMASK_BYTES + 2]
                            .try_into()
                            .unwrap(),
                    );
                    let compressed = fl & 0x8000 != 0;
                    let packed_len = (fl & 0x7FFF) as usize;
                    if !compressed {
                        uncompressed_chunks += 1;
                    }
                    offset += CHUNK_HEADER_SIZE + packed_len;
                }
            }
        }
        let groups_vec_bytes = self.groups.len() * std::mem::size_of::<Option<Box<Group>>>();
        let raw_bytes = self.node_count as usize * 8;
        let total_bytes = total_blob_bytes + groups_vec_bytes;
        eprintln!(
            "  SortedNodeStore: {} nodes, {} groups ({} used), {} chunks ({} uncompressed)",
            self.node_count, self.groups.len(), groups_used, total_chunks, uncompressed_chunks,
        );
        eprintln!(
            "  Blob data: {:.1} MB, Groups vec: {:.1} MB = {:.1} MB total (raw would be {:.1} MB, ratio {:.0}%)",
            total_blob_bytes as f64 / 1048576.0,
            groups_vec_bytes as f64 / 1048576.0,
            total_bytes as f64 / 1048576.0,
            raw_bytes as f64 / 1048576.0,
            100.0 * total_bytes as f64 / raw_bytes as f64,
        );

        SortedNodeStoreReader {
            groups: self.groups,
        }
    }

    /// Read coordinates (used by tests; not performance-critical).
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        let id = node_id as u64;
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
    pub fn node_count(&self) -> u64 {
        self.node_count
    }
}

/// Read-only sorted node coordinate store. Safe for concurrent reads
/// from rayon threads (all interior data is immutable).
pub struct SortedNodeStoreReader {
    groups: Vec<Option<Box<Group>>>,
}

impl SortedNodeStoreReader {
    /// Look up coordinates for a node. O(1) via bitmask popcount.
    /// Uses a thread-local decompression cache for amortized lookups.
    ///
    /// No #[hotpath::measure] — same reason as get_from_group_cached. At ~48M
    /// calls on Denmark, the instrumentation overhead (>50%) dwarfs actual cost.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        let id = node_id as u64;
        let group_id = (id / NODES_PER_GROUP) as usize;
        let chunk_id = ((id % NODES_PER_GROUP) / NODES_PER_CHUNK as u64) as u8;
        let node_in_chunk = (id % NODES_PER_CHUNK as u64) as u8;

        let group = self.groups.get(group_id)?.as_ref()?;
        get_from_group_cached(group, group_id, chunk_id, node_in_chunk)
    }
}

// ---------------------------------------------------------------------------
// NodeStore / NodeStoreReader — enum dispatch
// ---------------------------------------------------------------------------

/// Unified node store for the write phase.
#[allow(clippy::large_enum_variant)]
pub enum NodeStore {
    Flat(NodeIndex),
    Sorted(SortedNodeStore),
}

impl NodeStore {
    pub fn put(&mut self, node_id: i64, lat_e7: i32, lon_e7: i32) {
        match self {
            NodeStore::Flat(idx) => idx.put(node_id, lat_e7, lon_e7),
            NodeStore::Sorted(store) => store.put(node_id, lat_e7, lon_e7),
        }
    }

    pub fn into_reader(self) -> io::Result<NodeStoreReader> {
        match self {
            NodeStore::Flat(idx) => Ok(NodeStoreReader::Flat(idx.into_reader()?)),
            NodeStore::Sorted(store) => Ok(NodeStoreReader::Sorted(store.into_reader())),
        }
    }
}

/// Unified read-only node store for the parallel processing phase.
pub enum NodeStoreReader {
    Flat(NodeIndexReader),
    Sorted(SortedNodeStoreReader),
}

impl NodeStoreReader {
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        match self {
            NodeStoreReader::Flat(reader) => reader.get(node_id),
            NodeStoreReader::Sorted(reader) => reader.get(node_id),
        }
    }
}

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
        idx.put(100, 555_000_000, 133_000_000);
        assert_eq!(idx.get(100), Some((555_000_000, 133_000_000)));
    }

    #[test]
    fn put_multiple_get_each() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(10, 100_000, 200_000);
        idx.put(20, 300_000, 400_000);
        idx.put(30, 500_000, 600_000);
        assert_eq!(idx.get(10), Some((100_000, 200_000)));
        assert_eq!(idx.get(20), Some((300_000, 400_000)));
        assert_eq!(idx.get(30), Some((500_000, 600_000)));
    }

    #[test]
    fn get_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(5, 111, 222);
        assert_eq!(idx.get(999), None);
    }

    #[test]
    fn zero_zero_is_valid() {
        // Nodes at lat=0, lon=0 (Gulf of Guinea) should be stored and retrieved correctly.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(1, 0, 0);
        assert_eq!(idx.get(1), Some((0, 0)));
    }

    #[test]
    fn overwrite_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(42, 111_000, 222_000);
        idx.put(42, 333_000, 444_000);
        assert_eq!(idx.get(42), Some((333_000, 444_000)));
    }

    #[test]
    fn into_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(10, 100_000, 200_000);
        idx.put(20, 300_000, 400_000);

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

    // --- SortedNodeStore tests ---

    #[test]
    fn sorted_create_and_get_empty() {
        let store = SortedNodeStore::new();
        assert_eq!(store.get(1), None);
    }

    #[test]
    fn sorted_put_and_get() {
        let mut store = SortedNodeStore::new();
        store.put(100, 555_000_000, 133_000_000);
        assert_eq!(store.get(100), Some((555_000_000, 133_000_000)));
    }

    #[test]
    fn sorted_put_multiple_get_each() {
        let mut store = SortedNodeStore::new();
        store.put(10, 100_000, 200_000);
        store.put(20, 300_000, 400_000);
        store.put(30, 500_000, 600_000);
        assert_eq!(store.get(10), Some((100_000, 200_000)));
        assert_eq!(store.get(20), Some((300_000, 400_000)));
        assert_eq!(store.get(30), Some((500_000, 600_000)));
    }

    #[test]
    fn sorted_get_nonexistent() {
        let mut store = SortedNodeStore::new();
        store.put(5, 111, 222);
        assert_eq!(store.get(999), None);
    }

    #[test]
    fn sorted_zero_zero_is_valid() {
        let mut store = SortedNodeStore::new();
        store.put(1, 0, 0);
        assert_eq!(store.get(1), Some((0, 0)));
    }

    #[test]
    fn sorted_into_reader() {
        let mut store = SortedNodeStore::new();
        store.put(10, 100_000, 200_000);
        store.put(20, 300_000, 400_000);
        let reader = store.into_reader();
        assert_eq!(reader.get(10), Some((100_000, 200_000)));
        assert_eq!(reader.get(20), Some((300_000, 400_000)));
        assert_eq!(reader.get(999), None);
    }

    #[test]
    fn sorted_cross_chunk_boundary() {
        let mut store = SortedNodeStore::new();
        store.put(255, 10, 20);
        store.put(256, 30, 40);
        assert_eq!(store.get(255), Some((10, 20)));
        assert_eq!(store.get(256), Some((30, 40)));
    }

    #[test]
    fn sorted_cross_group_boundary() {
        let mut store = SortedNodeStore::new();
        store.put(65535, 10, 20);
        store.put(65536, 30, 40);
        let reader = store.into_reader();
        assert_eq!(reader.get(65535), Some((10, 20)));
        assert_eq!(reader.get(65536), Some((30, 40)));
    }

    #[test]
    fn sorted_large_gap_in_ids() {
        let mut store = SortedNodeStore::new();
        store.put(1_000_000, 10, 20);
        store.put(10_000_000, 30, 40);
        let reader = store.into_reader();
        assert_eq!(reader.get(1_000_000), Some((10, 20)));
        assert_eq!(reader.get(10_000_000), Some((30, 40)));
        assert_eq!(reader.get(5_000_000), None);
    }

    #[test]
    fn sorted_single_node() {
        let mut store = SortedNodeStore::new();
        store.put(42, 100, 200);
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
            store.put(i, i as i32 * 100, i as i32 * 200);
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
        store.put(12_000_000_000, 550_000_000, 130_000_000);
        let reader = store.into_reader();
        assert_eq!(reader.get(12_000_000_000), Some((550_000_000, 130_000_000)));
    }

    #[test]
    #[should_panic(expected = "strictly increasing")]
    fn sorted_rejects_non_monotonic() {
        let mut store = SortedNodeStore::new();
        store.put(10, 100, 200);
        store.put(5, 300, 400);
    }

    #[test]
    #[should_panic(expected = "strictly increasing")]
    fn sorted_rejects_duplicate_id() {
        let mut store = SortedNodeStore::new();
        store.put(10, 100, 200);
        store.put(10, 300, 400);
    }

    #[test]
    fn node_store_sorted_variant() {
        let mut store = NodeStore::Sorted(SortedNodeStore::new());
        store.put(100, 555_000_000, 133_000_000);
        let reader = store.into_reader().unwrap();
        assert_eq!(reader.get(100), Some((555_000_000, 133_000_000)));
        assert_eq!(reader.get(999), None);
    }

    #[test]
    fn node_store_flat_variant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut store = NodeStore::Flat(NodeIndex::create(&path).unwrap());
        store.put(100, 555_000_000, 133_000_000);
        let reader = store.into_reader().unwrap();
        assert_eq!(reader.get(100), Some((555_000_000, 133_000_000)));
        assert_eq!(reader.get(999), None);
    }
}
