# Box 2 Investigation: Node and Way Storage/Lookup

Deep code-level investigation of `src/node_index.rs`, `src/way_index.rs`, and their integration in `src/pipeline.rs`.

**Files analyzed:**
- `src/node_index.rs` (1173 lines) -- primary target
- `src/way_index.rs` (262 lines) -- secondary target
- `src/pipeline.rs` -- integration and access patterns
- `examples/bench_node_store.rs` -- synthetic benchmark
- `notes/hotpath-profile.md` -- measured performance data

---

## 1. SortedNodeStore (Primary Path)

### 1.1 Three-Level Hierarchy

The `SortedNodeStore` uses a fixed three-level hierarchy to decompose node IDs into group, chunk, and intra-chunk positions:

```
node_id (u64) decomposition:
  group_id     = node_id / 65536      (node_index.rs:643)
  chunk_id     = (node_id % 65536) / 256   -- u8, 0..255  (node_index.rs:644)
  node_in_chunk = node_id % 256             -- u8, 0..255  (node_index.rs:645)
```

Constants at `node_index.rs:152-154`:
```
NODES_PER_CHUNK = 256
NODES_PER_GROUP = 256 * 256 = 65536
BITMASK_BYTES = 32  (256 bits)
```

Each level uses a 256-bit bitmask for presence testing and popcount-based indexing into packed arrays.

### 1.2 Struct Layout

**`Group`** (`node_index.rs:278-281`):
```rust
struct Group {
    chunk_mask: [u8; 32],     // 256-bit bitmask: which of the 256 chunks have data
    data: Box<[u8]>,          // flat blob: all chunks' headers + packed data
}
// Size: 32 (mask) + 16 (Box<[u8]> = ptr + len) = 48 bytes + heap blob
```

**`SortedNodeStore`** (`node_index.rs:581-602`):
```rust
pub struct SortedNodeStore {
    groups: Vec<Option<Box<Group>>>,    // indexed by group_id, None = empty
    current_group_id: u64,
    current_chunk_mask: [u8; 32],
    current_group_data: Vec<u8>,        // blob being built for current group
    compress_buf: Vec<u8>,              // scratch for compression
    scratch_lats: Vec<i32>,             // scratch for flush_chunk
    scratch_lons: Vec<i32>,
    scratch_lat_offsets: Vec<u32>,
    scratch_lon_offsets: Vec<u32>,
    current_chunk_id: u8,
    current_node_mask: [u8; 32],
    current_coords: Vec<(i32, i32)>,
    last_node_id: i64,
    node_count: u64,
}
```

**`SortedNodeStoreReader`** (`node_index.rs:838-840`):
```rust
pub struct SortedNodeStoreReader {
    groups: Vec<Option<Box<Group>>>,
}
```

The reader is extremely simple -- it just takes ownership of the groups vector. All builder state is dropped.

**`CacheEntry`** (`node_index.rs:436-442`):
```rust
struct CacheEntry {
    group_id: usize,               // 8 bytes
    chunk_idx: usize,              // 8 bytes
    node_mask: [u8; 32],           // 32 bytes
    coords: [(i32, i32); 256],     // 2048 bytes (256 * 8)
    count: u16,                    // 2 bytes
}
// Total: ~2098 bytes, likely padded to 2104 with alignment
```

**`DecompressCache`** (`node_index.rs:467-469`):
```rust
struct DecompressCache {
    entries: [CacheEntry; 4],      // 4 * ~2104 = ~8416 bytes
}
```

### 1.3 Compression Scheme: Frame-of-Reference (FOR) Bitpacking

Each chunk stores up to 256 coordinate pairs. The compression is decided per-chunk at `node_index.rs:708`:

```
compressed = compress_buf.len() < raw_size  (raw_size = n * 8 bytes)
```

**Compressed chunk format** (`node_index.rs:283-285`):
```
[node_mask: 32 bytes]         -- which of 256 node positions have data
[flags_and_len: 2 bytes]      -- bit 15 = compressed flag, bits 0-14 = packed_len
[lat_min: 4 bytes (i32)]      -- minimum latitude in this chunk
[lon_min: 4 bytes (i32)]      -- minimum longitude in this chunk
[lat_bits: 1 byte]            -- bit width for latitude offsets
[lon_bits: 1 byte]            -- bit width for longitude offsets
[packed_lat: ceil(n * lat_bits / 8) bytes]
[packed_lon: ceil(n * lon_bits / 8) bytes]
```

The compression computes `lat_bits = required_bits(max(lat_offsets))` and `lon_bits = required_bits(max(lon_offsets))` where offsets are `value - min_value` (`node_index.rs:298-307`). This is a standard FOR (Frame of Reference) scheme.

**Uncompressed chunk format** (when FOR doesn't save space, `node_index.rs:717-722`):
```
[node_mask: 32 bytes]
[flags_and_len: 2 bytes]      -- bit 15 = 0 (not compressed), bits 0-14 = raw_size
[sequential (i32, i32) pairs]
```

**Compression effectiveness estimate:**
- For a dense chunk with 256 nodes in a local area, lat/lon offsets within a 65536-node-ID group will have limited range. A typical Denmark chunk might have coordinates spanning ~0.01 degrees of latitude = ~100,000 e7 units, requiring ~17 bits. With two coordinates at 17 bits each: `256 * 34 / 8 = 1088 bytes + 10 header = 1098` vs raw `256 * 8 = 2048 bytes`. ~46% reduction.
- Planet-scale chunks span wider geographic areas (nodes from a single 65536-ID group can be world-wide), so compression ratio will be worse.

The diagnostic output at `node_index.rs:781-792` confirms: Denmark blob data is ~270 MB total, compressed from a raw of ~420 MB (see hotpath profile).

### 1.4 Bitmask + Popcount O(1) Lookup -- Step-by-Step Walkthrough

To look up `node_id = 1000000`:

1. **Decompose ID** (`node_index.rs:851-853`):
   ```
   group_id = 1000000 / 65536 = 15
   chunk_id = (1000000 % 65536) / 256 = (16960) / 256 = 66
   node_in_chunk = 1000000 % 256 = 64
   ```

2. **Group lookup** (`node_index.rs:855`):
   ```rust
   let group = self.groups.get(15)?.as_ref()?;
   ```
   If `groups[15]` is `None`, return `None` immediately. This is O(1) -- direct vector indexing.

3. **Chunk presence test** (`node_index.rs:503`):
   ```rust
   if !test_bit(&group.chunk_mask, 66) { return None; }
   ```
   `test_bit` (`node_index.rs:164-166`): reads byte `66/8 = 8`, tests bit `66%8 = 2`. O(1).

4. **Chunk index via popcount** (`node_index.rs:506`):
   ```rust
   let chunk_idx = count_bits_before(&group.chunk_mask, 66);
   ```
   `count_bits_before` (`node_index.rs:171-177`): sums `popcount` of bytes 0..7, plus partial bits in byte 8. This gives the 0-based index into the packed array of present chunks. O(1) (constant-bounded: max 32 bytes).

5. **Cache lookup** (`node_index.rs:513-527`): Linear scan of 4 cache entries checking `group_id == 15 && chunk_idx == N`. On hit, uses cached `node_mask` and `coords`. LRU promotion via `entries.swap(0, i)`.

6. **On cache miss** (`node_index.rs:530-544`):
   - `find_chunk_in_blob` linearly scans the blob to find chunk at position `chunk_idx` (`node_index.rs:409-429`). Averages ~7 chunks per group in Denmark.
   - `decompress_chunk` unpacks coordinates into the evicted cache entry.
   - Node presence: `test_bit(&chunk.node_mask, 64)`.
   - Node index: `count_bits_before(&chunk.node_mask, 64)`.
   - Return `coords[node_idx]`.

**Theoretical complexity**: O(1) for group/chunk/node address decomposition, O(average_chunks_per_group) for blob scan on cache miss (bounded by 256). In practice, the dominant cost is DRAM latency on the blob data, not the scan itself.

### 1.5 Thread-Local LRU Cache

**Implementation** (`node_index.rs:456-489`):
- `thread_local!` with `UnsafeCell<DecompressCache>` -- chosen over `RefCell` to eliminate borrow-check overhead on ~48M calls/Denmark (`node_index.rs:484-486`).
- 4 entries per thread (`CACHE_ENTRIES = 4`, `node_index.rs:465`).
- LRU eviction: on hit, swap entry to position 0. On miss, swap last entry to front, overwrite it (`node_index.rs:521-523` for hit, `node_index.rs:537-538` for miss).
- `entries.swap()` is O(1) -- swaps `CacheEntry` structs by value but the compiler can use register/stack swaps, not memcpy of the 2KB coords arrays, since it's a simple swap of two array-in-struct values.

**What is cached**: Fully decompressed coordinate arrays. Each cache entry stores the complete decompressed `[(i32, i32); 256]` array plus the `node_mask` for presence testing. This means a cache hit avoids both blob scanning AND decompression.

**Cache hit rate**: The comments at `node_index.rs:403-406` state 76% hit rate on Denmark. This is consistent with the hotpath data: 23.8M decompress_chunk calls out of ~48M total lookups (from 6.6M ways * ~7.3 nodes/way average).

**Rayon interaction**: Each rayon thread gets its own independent `DecompressCache` via `thread_local!`. The cache is initialized lazily on first access per thread. With the default rayon pool size (e.g., 8 threads on plantasjen), there are 8 independent caches = 8 * 4 * ~2KB = ~64 KB total cache overhead. This is negligible.

**Why 4 entries**: The comment at `node_index.rs:457-464` explains the history:
- Started with 1 entry -- ways spanning 2 nearby chunks caused constant ping-ponging.
- 4 entries cut decompress_chunk calls by 17% (28.7M -> 23.8M) and avg latency by 42% (1.41us -> 820ns).
- The total decompress_chunk time dropped 52% (40.4s -> 19.5s).
- No evidence that 8 or 16 entries was tried. Diminishing returns are likely because OSM ways typically reference nearby nodes (within 1-3 chunks), so 4 covers the working set for most ways.

### 1.6 Memory Profile

**Group vector overhead** (`node_index.rs:778`):
```
groups_vec_bytes = groups.len() * size_of::<Option<Box<Group>>>()
```
`Option<Box<Group>>` is 8 bytes (pointer with niche optimization -- None is null pointer). For planet scale:
```
max_node_id ~ 12,000,000,000
max_group_id = 12B / 65536 = 183,105
groups_vec_bytes = 183,105 * 8 = ~1.4 MB
```
Negligible.

**Denmark (~52.5M nodes, ~65M actual from hotpath profile):**
- Blob data: 270 MB (from hotpath profile, confirmed in code at `node_index.rs:786`)
- Groups vec: < 1 MB
- Total: ~420 MB (stated in CLAUDE.md and hotpath profile)
- Raw would be: 52.5M * 8 = 420 MB -- so the FOR compression brings blob down to 270 MB, but groups vec and per-group Box overhead brings total to ~420 MB. Wait, the hotpath says total is 420 MB and raw would be 420 MB, so the compression ratio on the blob is ~64% (270/420).

Actually, let me re-derive from the diagnostic at `node_index.rs:781-792`:
```
  SortedNodeStore: {node_count} nodes, {groups.len()} groups ({used} used), {chunks} chunks
  Blob data: {blob_mb} MB, Groups vec: {vec_mb} MB = {total_mb} MB total (raw would be {raw_mb} MB, ratio {pct}%)
```

For Denmark 52.5M nodes: raw = 52.5M * 8 = 420 MB. The total (blob + groups vec) is reported as ~420 MB, which means the compression ratio is approximately 100%. This makes sense because "ratio" in the code is `total / raw * 100`, and 270 MB blob + ~150 MB overhead = ~420 MB.

Wait, that doesn't match. Let me re-read: the groups_vec is `groups.len() * 8`. For Denmark, if max node ID is around 12B (global IDs, even for a Denmark extract), then `groups.len()` could be up to 183K = 1.4 MB. But the MEMORY.md says "SortedNodeStore: ~420 MB in-RAM". The hotpath says "Blob data: 270 MB". So the overhead must be elsewhere -- possibly the `Box<Group>` allocations themselves (48 bytes per group * N groups).

**Planet scale (~8.5B nodes):**
- Raw: 8.5B * 8 = 68 GB
- Groups vec: 183K * 8 = 1.4 MB
- Number of groups used: 8.5B / 65536 nodes_per_group = ~130K groups (if uniformly distributed; in practice many groups will have gaps). Planet has nodes everywhere, so most of the 183K possible groups will be populated.
- Blob data estimate: With planet-scale data, compression ratio will be worse because nodes within a single 65536-ID group can span very different geographic areas (IDs are assigned chronologically by OSM, not geographically). Estimate: maybe 60-80% compression ratio, so 41-54 GB blob. Per MEMORY.md's estimate: ~51 GB.
- **Critical finding**: 51 GB + overhead is likely to exceed 64 GB RAM.

### 1.7 Access Patterns and Locality

**Way node references are geographically local**: In OSM, ways reference nodes that are physically nearby (a road's nodes are close together). However, OSM node IDs are assigned chronologically, not geographically. Nearby geographic nodes may have very different IDs, assigned across different edit sessions.

**Practical locality**: Despite the ID assignment policy, there IS temporal locality. Most ways were created in a single edit session, and their nodes were created in the same session with nearby IDs. The 76% cache hit rate on Denmark confirms that consecutive node refs within a way frequently fall within the same chunk (256 consecutive IDs) or a nearby chunk.

**Planet-scale locality degradation**: At planet scale, ways from early OSM history (low IDs) reference nodes with small IDs that are close together. But later edits create new nodes with high IDs that might be geographically close to old nodes but in completely different chunks. The cache hit rate will likely decrease for ways in heavily-edited areas. However, the 4-entry cache should still catch the most common case (2-3 chunks per way).

---

## 2. Flat Node Index (Fallback Path)

### 2.1 Structure

**`NodeIndex`** (`node_index.rs:34-38`):
```rust
pub struct NodeIndex {
    file: File,        // backing file
    mmap: MmapMut,     // writable memory-mapped view
    file_len: u64,     // current file size
}
```

**Direct addressing** (`node_index.rs:68`):
```
offset = node_id * 8  (ENTRY_SIZE = 8 bytes: 4 lat_e7 + 4 lon_e7)
```

**Sentinel handling** (`node_index.rs:27-32`): Coordinates are XOR'd with `0x55555555` before storage so that a real `(0, 0)` node stores as non-zero, and all-zeros on disk means "unset". Clever: 0x55555555 as latitude = 143.17 degrees, outside valid range.

### 2.2 Grow Logic

**Trigger** (`node_index.rs:71`): `needed > self.file_len` where `needed = node_id * 8 + 8`.

**Growth** (`node_index.rs:72-81`):
```rust
let mut new_len = self.file_len;
while new_len < needed {
    new_len += GROW_INCREMENT;   // 1 GB = 1,073,741,824 bytes
}
self.file.set_len(new_len)?;
self.mmap = MmapMut::map_mut(&self.file)?;
```

Each grow involves:
1. `ftruncate()` syscall to extend the file
2. `munmap()` of old mapping (implicit in MmapMut drop)
3. `mmap()` of the new, larger file

**Initial size**: 1 GB (`node_index.rs:53`).

**Number of grows for planet scale**: Max node ID ~12B. File size needed: 12B * 8 = 96 GB. Starting at 1 GB, growing in 1 GB steps: 96 grows. Each grow remaps the entire file, which means 96 `munmap` + `mmap` syscalls during node insertion.

### 2.3 Why This is Catastrophic at Planet Scale

**File size**: 12B * 8 = 96 GB. This is a sparse file -- only populated node IDs have actual disk pages allocated. For planet (~8.5B nodes), actual on-disk usage would be ~68 GB. But the virtual address space mapping is 96 GB.

**Page cache**: On a 64 GB machine, the 96 GB sparse file cannot be fully cached. During way processing, random node lookups will trigger page faults. The OS must evict existing pages to service new reads.

**Page fault pattern**: Ways reference nodes scattered across the 96 GB address space. Each lookup is `mmap[node_id * 8]`, which is a random 4 KB page access. With 8.5B nodes spread across 96 GB, the "density" is ~8.5B * 8 / 96 GB = ~71% of pages populated. But the way processing accesses nodes in a pattern that is neither sequential nor fully random -- it's clustered by geographic area with temporal mixing.

**Measured catastrophe** (from `notes/hotpath-profile.md:153`): On dm6 with the sparse mmap, the total pipeline time was 242s for Denmark. After switching to SortedNodeStore, it dropped to 17.2s -- a 14x improvement. Denmark only has ~52.5M nodes with a sparse file of ~96 GB; at planet scale the situation would be far worse because:
1. The sparse file has node IDs from 1 to 12B, meaning 96 GB virtual mapping regardless of extract size.
2. Even Denmark's 52.5M nodes are spread across the full 12B ID range.
3. At planet scale, 68 GB of actual data in 64 GB RAM means continuous eviction.

**The madvise disaster** (documented in CLAUDE.md): All madvise hints were tried and caused severe regressions. `MADV_HUGEPAGE` caused a 3.5x slowdown (45s -> 160s on dm6) because the kernel tried to assemble 2 MB huge pages from a file that is 99%+ holes. The guard condition checked `file_len` (96 GB) instead of actual working set (~400 MB), so it never triggered. Lesson: sparse files and memory hints don't mix.

### 2.4 Read Path

**`into_reader()`** (`node_index.rs:100-104`):
```rust
pub fn into_reader(self) -> io::Result<NodeIndexReader> {
    let file_len = self.file_len;
    let mmap = self.mmap.make_read_only()?;
    Ok(NodeIndexReader { mmap, file_len })
}
```

Converts `MmapMut` to read-only `Mmap` (which is `Sync`), enabling concurrent rayon access. The `make_read_only()` call invokes `mprotect()` to change page permissions from RW to RO.

**`get()`** (`node_index.rs:109-127`): Direct addressing. Read 8 bytes at `node_id * 8`, XOR-unmask, return if non-zero.

---

## 3. Way Index

### 3.1 Structure

**`WayIndex`** (`way_index.rs:16-29`):
```rust
pub struct WayIndex {
    // Offset index: mmap'd, indexed at way_id * 12
    offsets_file: File,
    offsets_mmap: MmapMut,
    offsets_file_len: u64,

    // Data file: buffered writer during write, mmap after finish
    data_writer: Option<BufWriter<File>>,
    data_write_pos: u64,
    data_path: PathBuf,

    // Read-only mmap over data file, set after finish_writing()
    data_mmap: Option<Mmap>,
}
```

Two files:
1. **`way_offsets.bin`**: Direct-addressed at `way_id * 12`. Each entry is 8 bytes `data_offset` + 4 bytes `coord_count` = 12 bytes (`ENTRY_SIZE`, `way_index.rs:7`).
2. **`way_data.bin`**: Append-only packed coordinates, `(i32, i32)` pairs at 8 bytes each (`COORD_SIZE`, `way_index.rs:9`).

### 3.2 Write Path

**`put()`** (`way_index.rs:74-110`):
1. Appends coordinate bytes to `data_writer` (BufWriter over `way_data.bin`).
2. Records `(data_offset, coord_count)` into `offsets_mmap` at `way_id * 12`.
3. Grows offsets file in 1 GB increments if needed (same pattern as flat node index).

**Important: per-coordinate writes** (`way_index.rs:83-86`):
```rust
for &(lat_e7, lon_e7) in coords {
    writer.write_all(&lat_e7.to_le_bytes())?;
    writer.write_all(&lon_e7.to_le_bytes())?;
}
```
Each coordinate pair is two 4-byte writes through BufWriter. The BufWriter absorbs this into its 8 KB default buffer, so actual syscalls are infrequent.

**`finish_writing()`** (`way_index.rs:114-128`): Flushes BufWriter, opens a read-only mmap over `way_data.bin`.

### 3.3 Read Path

**`get()`** (`way_index.rs:134-165`):
```rust
pub fn get(&self, way_id: i64) -> Option<&[(i32, i32)]> {
    let index_offset = way_id as u64 * ENTRY_SIZE;
    // ... bounds check ...
    let data_offset = u64::from_le_bytes(...);
    let coord_count = u32::from_le_bytes(...);
    // Sentinel: (0, 0) = unset
    if data_offset == 0 && coord_count == 0 { return None; }
    let mmap = self.data_mmap.as_ref()?;
    let bytes = &mmap[start..start + byte_len];
    // Safety: cast bytes to (i32, i32) slice
    let ptr = bytes.as_ptr().cast::<(i32, i32)>();
    Some(unsafe { std::slice::from_raw_parts(ptr, coord_count as usize) })
}
```

Zero-copy: returns a slice directly into the mmap'd data file. The `unsafe` cast from `&[u8]` to `&[(i32, i32)]` is guarded by compile-time assertions at `way_index.rs:12-14` (size, alignment, endianness).

### 3.4 Memory Profile

**Offsets file**:
- Denmark: max way_id maybe ~1.2B (global IDs). File size: 1.2B * 12 = ~14.4 GB (sparse).
- Planet: max way_id maybe ~1.5B. File size: 1.5B * 12 = ~18 GB (sparse).
- Actual populated pages: Denmark ~6.6M ways * 12 = ~79 MB. Planet ~800M ways * 12 = ~9.6 GB.

**Data file**:
- Denmark: ~6.6M ways, average ~7.3 nodes/way = ~48M coords * 8 bytes = ~384 MB.
- Planet: ~800M ways, average ~7 nodes/way = ~5.6B coords * 8 bytes = ~44.8 GB.

**Important**: The way index is only used during the PBF read phase for relation processing. After `phase_read_and_process` completes, it's dropped explicitly (`pipeline.rs:596`):
```rust
drop(way_index);  // "frees ~11 GB from RSS for North America"
```

### 3.5 Sentinel Bug (Minor)

At `way_index.rs:233-243`: An empty way (0 coordinates) that is the first insertion gets `(data_offset=0, coord_count=0)`, which is indistinguishable from "unset". The test documents this edge case. In practice, empty ways are filtered out before way_index.put() is called (`pipeline.rs:717`: `if !pw.coords_e7.is_empty()`).

---

## 4. Pipeline Integration

### 4.1 Node Store Selection

**`pipeline.rs:335-346`**:
```rust
let is_sorted = reader.header().is_sorted() || config.force_sorted;
let mut node_store_opt: Option<NodeStore> = Some(if is_sorted {
    NodeStore::Sorted(SortedNodeStore::new())
} else {
    NodeStore::Flat(NodeIndex::create(&idx_dir.join("nodes.idx"))?)
});
```

The decision is binary: either the PBF header declares `Sort.Type_then_ID` (or `--force-sorted` is set), using the compact in-RAM store, or the flat mmap fallback is used. There is no runtime detection or hybrid approach.

### 4.2 Node Write Phase (Nodes)

Nodes are processed inline on the main thread via the `handle_node!` macro (`pipeline.rs:385-415`):
```rust
node_store_opt.as_mut()...put(node.id(), lat_e7, lon_e7);
```

For sorted PBFs, this builds the SortedNodeStore incrementally. The `put()` asserts strictly increasing IDs (`node_index.rs:634-638`).

### 4.3 Node Store Transition

When the first Way block arrives, the node store is converted from write to read mode (`pipeline.rs:441-445`):
```rust
let ns = node_store_opt.take()...;
let nr = Arc::new(ns.into_reader()...);
```

For `SortedNodeStore`, `into_reader()` (`node_index.rs:749-797`):
1. Flushes current chunk and group
2. Prints diagnostic statistics
3. Moves `groups` Vec into `SortedNodeStoreReader`

For `NodeIndex`, `into_reader()` (`node_index.rs:100-104`):
1. Converts `MmapMut` to read-only `Mmap` via `make_read_only()`

The `Arc<NodeStoreReader>` is cloned into the worker thread (`pipeline.rs:450`), enabling concurrent reads from rayon threads.

### 4.4 Node Lookup During Way Processing

**Call chain**: `process_raw_way` (`pipeline.rs:733-806`) -> `node_reader.get(id)` (`pipeline.rs:745`) -> `SortedNodeStoreReader::get()` (`node_index.rs:849-857`) -> `get_from_group_cached()` (`node_index.rs:497-546`).

The lookup happens inside a rayon parallel iterator (`pipeline.rs:490-495`):
```rust
let results: Vec<ProcessedWay> = raw_ways
    .into_par_iter()
    .map(|raw| process_raw_way(&raw, nr_ref, lm_ref, mz, xz))
    .collect();
```

Inside `process_raw_way`, ALL node refs are resolved before any geometry processing:
```rust
let coords_e7: Vec<(i32, i32)> = raw.node_refs.iter()
    .filter_map(|&id| node_reader.get(id))
    .collect();
```

This means each way's node lookups happen sequentially within a single rayon task. The thread-local cache benefits from temporal locality within a single way (consecutive node refs often in same/nearby chunks).

### 4.5 Way Index Access During Relations

Relations use the way index to look up member way geometries (`pipeline.rs:857`):
```rust
if let Some(coords_e7) = way_index.get(way_id) {
    let merc: Vec<Point> = coords_e7.iter()
        .map(|&(lat, lon)| geometry::project_e7(lat, lon))
        .collect();
    member_ways.push(MemberWay { role, coords: merc });
}
```

This is called from `prepare_relation` on the main thread (serial). The `way_index.get()` returns a zero-copy slice into the data mmap. The coordinates are then immediately projected to Mercator and collected into an owned Vec.

---

## 5. Verification of Theoretical Review Claims

### Claim 1 (Critical): "Flat fallback path is highly sensitive to sparse-ID page-fault behavior at planet scale"

**VERIFIED and ELABORATED.** The flat path creates a file sized at `max_node_id * 8` bytes. For planet, this is 96 GB. With 64 GB RAM, the page cache cannot hold the file. Random way-node lookups will cause severe page fault thrashing. The measured 14x regression on Denmark (242s -> 17.2s when switching to SortedNodeStore, per hotpath profile) confirms this is catastrophic. At planet scale it would be far worse.

The review's anchors at `pipeline.rs:345` (flat path selection) and `node_index.rs:25,53` (GROW_INCREMENT and initial size) are correct.

**Additional detail**: The flat path also creates a file on disk in `tilegen_tmp/`. At planet scale, the 96 GB file competes with way index files for disk I/O bandwidth. Even if the file is sparse (only populated pages use disk), the `ftruncate` to 96 GB allocates metadata for the full range.

### Claim 2 (High): "Sorted reader uses cache of only 4 entries; heavy cross-chunk access skew could thrash"

**PARTIALLY VERIFIED -- NUANCED.** The cache is indeed 4 entries (`node_index.rs:465`). However:

1. **76% hit rate on Denmark** (`node_index.rs:403-406`) -- substantially better than the review implies.
2. **The cache is thread-local** -- each rayon thread has its own 4-entry cache. No cross-thread contention.
3. **Ways reference nearby nodes** -- consecutive node refs in a way typically span 1-3 chunks. 4 entries comfortably covers this.
4. **The real bottleneck is DRAM, not cache policy** -- even on a miss, `decompress_chunk` at 560ns (plantasjen) or 820ns (dm6) is dominated by DRAM fetch latency of the blob data. Making the cache larger would increase hit rate marginally but wouldn't reduce miss latency.

**Where thrashing could occur**: Relations with many member ways from different geographic areas. The relation processing runs serially on the main thread, but uses `get_from_group` (no cache) through the way_index, not the node store cache. So relation processing doesn't interact with the node cache at all.

**Planet-scale risk**: At planet scale, the blob grows from 270 MB to ~50 GB. The L3 cache (64 MB on plantasjen) holds <0.2% of the blob. Every cache miss will hit DRAM (~100ns) or worse, page cache eviction if the blob doesn't fit in RAM. The 4-entry decompression cache saves re-decompression but cannot help with DRAM fetch latency.

**Verdict**: The 4-entry cache size is not the problem. The problem is blob size exceeding CPU cache hierarchy. More decompression cache entries (e.g., 8 or 16) would have diminishing returns because the dominant cost is fetching blob data from DRAM, not re-decompressing.

### Claim 3 (Medium): "1 GB grow increments on sparse mmaps create abrupt VMA growth"

**VERIFIED but LOW IMPACT.** The grow logic (`node_index.rs:72-81`, `way_index.rs:93-105`) does 1 GB increments. For the flat node index, this means up to 96 grows at planet scale. Each grow calls `ftruncate` + `mmap`, which:
1. Extends the file's logical size (fast -- just metadata)
2. Creates/replaces a VMA in the process's address space

The "noisy memory accounting" concern is valid -- `top`/`htop` will show the full sparse file size as virtual memory, which is confusing but not harmful. The actual page fault cost depends on access patterns, not the VMA size.

**However**: The grow only happens during the write phase (node insertion), which is sequential on the main thread. Once converted to a reader, no more grows occur. The 96 grows during a planet-scale node phase add negligible overhead compared to the actual node insertion work.

**For way_index**: Similar analysis. The offsets file grows to ~18 GB for planet, requiring ~18 grows. Again, negligible overhead.

---

## 6. What the Review Missed

### 6.1 Planet-Scale SortedNodeStore Memory Crisis

The theoretical review focused on the flat fallback but **underestimated the SortedNodeStore's own planet-scale problem**. From `notes/hotpath-profile.md:146-147`:

> SortedNodeStore for planet (8.5B nodes * 8 bytes = 68 GB uncompressed) won't fit in 64 GB RAM -- needs bitpacked coordinate compression (~51 GB estimate).

Even with FOR compression, ~51 GB for the blob plus groups vector overhead may not fit in 64 GB alongside the way index, sort writer buffers, and OS needs. This is arguably more critical than the flat fallback issue, because the sorted path IS the production path, and it doesn't work at planet scale on a 64 GB machine.

The current compression (FOR within chunks) achieves ~64% ratio on Denmark (270 MB blob / 420 MB raw). At planet scale:
- Raw: 68 GB
- With current FOR: ~43-51 GB (lower compression on geographically-scattered planet data)
- Plus groups vec: ~1.4 MB (negligible)
- Plus `Box<Group>` allocations: ~130K * 48 bytes = ~6 MB (negligible)
- **Total estimate: 44-52 GB**

This leaves 12-20 GB for everything else (way index, sort chunks, rayon thread stacks, OS). The way index data file alone could be 45 GB at planet scale. This means both data structures cannot coexist in memory simultaneously.

**Saving grace**: The node store is read-only during way processing, and way_index is written serially on the drain thread during way processing but only read during relation processing. They DO coexist during the way phase, but way_index data is written through BufWriter (sequential I/O, kernel can manage page cache efficiently) while node store is random-accessed.

### 6.2 `find_chunk_in_blob` Linear Scan Cost at Planet Scale

The blob scan at `node_index.rs:409-429` iterates through `chunk_idx` chunks to find the target. Comment says "Denmark averages ~7 chunks/group" so the scan is short. At planet scale with denser groups:
- Dense groups (all 256 chunks present): scan of 256 * (32 + 2 = 34) = 8704 bytes to reach the last chunk.
- Average for planet: maybe 50-100 chunks/group (65536 nodes / group, ~130K groups for 8.5B nodes = ~65K nodes/group = ~254 chunks/group on average for populated groups).

With 254 chunks per group, a scan touching ~254 * 34 = 8.6 KB of blob data per miss becomes significant. However, with 76%+ cache hit rate, this only runs on 24% of lookups. And the blob scan data is sequential (good for prefetcher).

**Potential optimization**: Pre-compute chunk offset table per group. The code comment at `node_index.rs:403-406` says this was tried and reverted because of the high hit rate. At planet scale with larger groups, the cost/benefit might change.

### 6.3 `into_reader()` Creates a Memory Spike

When `SortedNodeStore::into_reader()` is called (`node_index.rs:749-797`), it runs diagnostics that scan the entire blob:
```rust
for g in self.groups.iter().flatten() {
    total_blob_bytes += g.data.len();
    let mut offset = 0;
    while offset < g.data.len() {
        // scan every chunk header
    }
}
```

This is a full sequential scan of all blob data -- at planet scale, 51 GB of sequential reads. The diagnostic is harmless at Denmark scale but could add noticeable latency at planet scale. This is a one-time cost at the node-to-way transition.

### 6.4 No Warning for Planet-Scale Flat Fallback

When the PBF is not sorted and the flat path is selected (`pipeline.rs:344`), there is no warning about the expected file size or memory impact. The message is just:
```
PBF not sorted -- using flat mmap node index
```

At planet scale, this should emit a prominent warning or even abort with a suggestion to use `--force-sorted`.

### 6.5 Way Index Sentinel Edge Case

At `way_index.rs:149-152`, unset detection uses `(data_offset == 0 && coord_count == 0)`. If a way has 0 coordinates AND is the first way written (data_write_pos = 0), the entry is indistinguishable from "unset". The pipeline guards against this (`pipeline.rs:717`: `if !pw.coords_e7.is_empty()`), but if a future code path bypasses this check, way 0 or the first-written empty way would silently disappear. This is documented in tests but is a latent correctness risk.

### 6.6 Way Index Data File Size at Planet Scale

The way data file stores all way coordinates: ~5.6B coords * 8 bytes = ~44.8 GB. Combined with the offsets file (18 GB sparse, ~9.6 GB populated), the way index alone requires ~54 GB. On a 64 GB machine with a 51 GB node store also in memory, this is impossible.

However, the way index data file is mmap'd read-only only after `finish_writing()` (`way_index.rs:121-124`), and is only read during relation processing. The node store and way index do coexist during the way processing phase, but the way index write is sequential (good for page cache), and the data mmap isn't opened until relations start (after ways finish).

**Timeline of memory usage**:
1. Node phase: SortedNodeStore grows to 51 GB.
2. Way phase: SortedNodeStore (51 GB, read-only) + way_index offsets (18 GB sparse, writes) + way_data (sequential writes, BufWriter).
3. Transition: way_index.finish_writing() opens data_mmap (45 GB).
4. Relation phase: SortedNodeStore (51 GB) + way_index offsets (18 GB) + way_data mmap (45 GB) = 114 GB virtual, ~106 GB actual.
5. After phase_read_and_process: way_index dropped (pipeline.rs:596), SortedNodeStore dropped (it's consumed by sort_writer already).

Wait -- actually the SortedNodeStore is wrapped in `Arc<NodeStoreReader>` during the way phase (`pipeline.rs:443`), and the Arc is moved into the worker thread. After the worker and drain threads join (`pipeline.rs:530-538`), the Arc is dropped. But the node store is NOT explicitly dropped before relation processing. Let me check...

Looking at `pipeline.rs:441-445`: `node_store_opt.take()` takes ownership, converts to reader, wraps in `Arc`. The Arc is `nr` and `nr_clone`. `nr_clone` goes to the worker thread. When worker and drain join (`pipeline.rs:530-538`), the worker drops its Arc clone. But `nr` (the original) is still in scope until the function returns.

Actually, `nr` is declared in the `if block_tx.is_none()` block (`pipeline.rs:443`), which means it has block scope. It will be dropped when that block ends... but wait, it's referenced by `nr_clone` in the worker thread. The Arc semantics handle this correctly -- when both the worker thread's clone and the local `nr` are dropped, the underlying reader is deallocated.

**Key question**: Is the SortedNodeStoreReader alive during relation processing?

Looking more carefully: `nr` is created at `pipeline.rs:443` inside the `if block_tx.is_none()` block, which is inside the `BlockType::Ways` arm. After the block scope, `nr` is dropped. `nr_clone` is moved into the worker thread closure. When the worker thread is joined (`pipeline.rs:531`), its closure is dropped, which drops `nr_clone`, which drops the last Arc, which drops the `SortedNodeStoreReader`.

So by the time relations start processing, the SortedNodeStoreReader IS dropped. During relation processing, only way_index is needed.

**Revised timeline**:
1. Node phase: SortedNodeStore grows (51 GB for planet).
2. First Way block: SortedNodeStore converted to reader (51 GB), way_index starts writing.
3. Way phase: NodeStoreReader (51 GB read-only) + way offsets + way data (write).
4. First Relation block (or PBF end): Worker + drain join. NodeStoreReader dropped (51 GB freed). Way_index finalized (data mmap opened: 45 GB).
5. Relation phase: way_index only (offsets: 18 GB sparse, data: 45 GB mmap).
6. After phase_read_and_process: way_index dropped.

This means peak memory during the way phase is: 51 GB (node store) + way offsets (~10 GB populated of 18 GB sparse) + way data BufWriter (8 KB buffer). Total: ~61 GB. This is dangerously close to 64 GB limit for planet.

### 6.7 Hidden Allocation in `process_raw_way`

At `pipeline.rs:744-746`:
```rust
let coords_e7: Vec<(i32, i32)> = raw.node_refs.iter()
    .filter_map(|&id| node_reader.get(id))
    .collect();
```

This allocates a new Vec per way. The comment at `pipeline.rs:741-743` explains why it can't be hoisted: "ownership transfers into ProcessedWay for the serial way_index.put() phase, so a reusable buffer would need .to_vec()/.clone() anyway."

At planet scale with ~800M ways at ~7 nodes each: 800M allocations of ~56 bytes each = ~44 GB of allocator throughput just for coords_e7 Vecs. However, with mimalloc's thread-local pools, this is fast.

### 6.8 The `groups` Vec Contains Many `None` Entries

At `node_index.rs:649-651`:
```rust
if self.groups.len() <= group_id as usize {
    self.groups.resize_with(group_id as usize + 1, || None);
}
```

For a Denmark extract, nodes may have IDs up to 12B, giving `group_id` up to 183K. But Denmark only has ~52.5M nodes, using maybe ~800 distinct groups. The groups Vec has 183K entries, of which 182K are `None` (8 bytes each). That's 1.4 MB wasted -- negligible.

At planet scale: max group_id ~183K, most populated. The Vec is a contiguous allocation of ~1.4 MB. No concern.

### 6.9 `Vec::with_capacity(NODES_PER_CHUNK)` Leak in Group/Chunk Transitions

At `node_index.rs:666` and `node_index.rs:672`:
```rust
self.current_coords = Vec::with_capacity(NODES_PER_CHUNK);  // 256 * 8 = 2 KB
```

This creates a new Vec for each new chunk, dropping the old one. On Denmark with ~205K chunks, this is 205K alloc/dealloc cycles of 2 KB each. The scratch Vecs (lats, lons, offsets) are hoisted and reused (`node_index.rs:679-681`), but `current_coords` is recreated. The comment at `node_index.rs:681` says hoisting the scratch buffers gave -5.3% build time and -6.8% way-like lookup time.

**Why isn't `current_coords` hoisted?** It could be: clear the existing Vec instead of creating a new one. The `.clear()` call is already there at `node_index.rs:725`. The issue is that after `flush_chunk`, the old Vec's data has already been consumed (coordinates copied to compress buffers), so `.clear()` would work. But the code creates a new Vec at the group/chunk transition points instead.

Wait, looking more carefully: at `node_index.rs:666`:
```rust
self.current_coords = Vec::with_capacity(NODES_PER_CHUNK);
```
This replaces the existing Vec (dropping it). At `node_index.rs:725`:
```rust
self.current_coords.clear();
```
This clears the same Vec without dropping it.

The clear-at-line-725 handles the same-group-same-chunk case (after flush_chunk). But lines 666 and 672 handle group/chunk transitions where the Vec is replaced. These could be `self.current_coords.clear()` instead, saving the alloc/dealloc. This is a minor optimization opportunity (~205K avoided allocations for Denmark, ~25M for planet).

---

## 7. Summary of Findings by Severity

### Critical

1. **SortedNodeStore cannot fit in 64 GB at planet scale.** The blob data alone is estimated at 44-52 GB. Combined with way index memory during the way phase, peak RSS approaches or exceeds 64 GB. This is the blocking issue for planet-scale support.

2. **Flat fallback is unusable at planet scale.** Already known and documented. 96 GB sparse file, 14x measured regression even on Denmark. No runtime warning.

### High

3. **Way index planet-scale memory.** During relation processing, way offsets (18 GB sparse) + way data (45 GB mmap) = ~55 GB. This is feasible alone but tight.

4. **Peak memory during way phase** = node store + way index writes. At planet scale: ~51 GB + ~10 GB = ~61 GB, leaving only ~3 GB for sort buffers, rayon stacks, and OS.

### Medium

5. **`find_chunk_in_blob` linear scan scales with chunks/group.** At planet scale with ~254 chunks/group average, the scan touches ~8.6 KB per miss. With 24% miss rate and ~48M lookups (way-like), that's ~11.5M scans * ~4 KB average = ~46 GB of blob data scanned during way phase. Most will be cache-cold at planet scale.

6. **No runtime diagnostics for memory pressure.** No RSS tracking, no warning when SortedNodeStore exceeds a threshold, no abort when approaching machine limits.

7. **Minor alloc churn in current_coords Vec replacement** at group/chunk transitions. ~205K unnecessary alloc/dealloc on Denmark, ~25M on planet.

### Low

8. **`into_reader()` diagnostic scan** reads entire blob sequentially. One-time cost, but at 51 GB, could add seconds at planet scale.

9. **Way index sentinel edge case** (`data_offset=0, coord_count=0`) -- guarded in practice but latent.

10. **1 GB grow increments** -- theoretically noisy but practically negligible overhead.

---

## 8. Opportunities Not Identified by the Review

### 8.1 Streaming/Spilling SortedNodeStore

Instead of keeping the entire blob in RAM, completed groups could be spilled to a memory-mapped file. Groups are immutable after creation, so they're perfect candidates for mmap'ing. This would reduce peak RSS from ~51 GB to working-set size. The decompression cache would still work -- it caches decompressed chunks regardless of whether the blob is in-RAM or mmap'd.

**Trade-off**: Adds I/O during way phase lookups. But with 76% cache hit rate, only 24% of lookups touch the blob. Sequential way-processing locality means the page cache would keep hot groups resident.

### 8.2 Geographic Reordering of Chunks

If node coordinates were clustered geographically within groups (e.g., by sorting the groups Vec by average coordinate), cache behavior might improve. However, group assignment is determined by node ID (chronological), not geography. Changing this would require a full restructuring.

### 8.3 Larger Chunk Size with Lazy Decompression

Instead of decompressing the full 256-entry chunk on every access, use delta-coded chunks with position-based seeking. This would reduce decompression cost per lookup at the expense of more complex code.

### 8.4 Way Index Compression

The way data file stores raw coordinates at 8 bytes per point. FOR compression (same scheme as SortedNodeStore) could reduce this significantly, since way coordinates are geographically close. For planet: potential 2-3x compression, reducing 45 GB to 15-22 GB. Trade-off: decompression cost during relation processing.

### 8.5 Combined Node+Way Store

Instead of two separate data structures, a single hierarchical store keyed by entity ID could share the groups/chunks infrastructure and reduce total memory overhead. This is a major refactor.
