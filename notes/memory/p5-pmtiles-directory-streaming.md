# P5: PMTiles Directory Finalization Streaming Redesign

## Status: IMPLEMENTED (`d11744e`)

Both steps complete:
- **Step 1**: Fixed double-buffer in `collect_dir_entries` — replaced `read_to_end` + manual parse
  with `read_dir_entries()` helper using `BufReader` + `read_exact` (24 bytes at a time)
- **Step 2**: Added `finalize_directories()` — streaming mode reads `dir_entries.bin` in chunks of
  4096 entries, builds leaf directories incrementally, never materializes full `Vec<DirEntry>`.
  In-memory mode delegates to existing `build_leaf_directories()`. Small datasets (≤16384 entries)
  fall through to root-only path.
- Removed dead `read_u64_le`/`read_u32_le` helpers and old `build_directories` method
- Added tests: `write_to_streaming_produces_valid_header`, `write_to_streaming_matches_in_memory`

Planet-scale finalization peak: ~240 MB → ~30 MB (leaf blob only).
Denmark/Germany benchmarks (dm6, `d11744e`): no regression, output byte-identical.

## Detailed Analysis and Implementation Plan

### 1. Current Architecture Summary

The PMTiles writer (`/home/folk/Programs/elivagar/src/pmtiles_writer.rs`) operates in two modes:

**In-memory mode** (`PmtilesWriter::new`): Both tile blob and directory entries stored in `Vec`. Used for tests and small extracts.

**Streaming mode** (`PmtilesWriter::new_streaming`): Tile blob written to `tiles.blob` temp file. Directory entries serialized to `dir_entries.bin` temp file at 24 bytes per run-length-encoded entry. This is the planet-scale path.

The `DirEntry` struct is exactly 24 bytes (compile-time asserted at line 70):
```rust
struct DirEntry {
    tile_id: u64,   // 8 bytes
    offset: u64,    // 8 bytes
    length: u32,    // 4 bytes
    run_length: u32, // 4 bytes
}
```

### 2. How Entries Accumulate

During `add_tile()` (line 189), each tile is first checked against the dedup map. Then `push_dir_entry()` (line 331) is called. This method performs run-length encoding: if the new tile has a consecutive Hilbert ID and the same (offset, length) as the current run, the run is extended. Otherwise, the old run is flushed to the `DirStore` (either appended to the in-memory Vec or serialized to the temp file) and a new run begins.

**Key insight**: Run-length encoding collapses consecutive dedup'd tiles into a single entry. Ocean fill tiles benefit enormously. But at high zoom levels (z13-z14), most tiles are unique with `run_length=1`, so each tile generates one directory entry.

### 3. The Finalization Memory Spike (Current)

`write_to()` (line 235) performs:

1. **Drops dedup map** (line 237) -- frees ~50 MB. **Already implemented (F6).**

2. **`collect_dir_entries()`** (line 239) -- In streaming mode, this:
   - Flushes the BufWriter
   - Reads the entire `dir_entries.bin` via `file.read_to_end(&mut data)` into a `Vec<u8>` (planet: ~120 MB)
   - Parses into `Vec<DirEntry>` (planet: ~120 MB)
   - Both exist simultaneously: **~240 MB transient spike**
   - The `Vec<u8>` is dropped when the match arm exits, leaving ~120 MB in `Vec<DirEntry>`

3. **`build_directories(&entries)`** (line 242) -- Takes the full `&[DirEntry]` slice. For >16384 entries, calls `build_leaf_directories()` which:
   - Iterates `entries` in chunks of 4096
   - Each chunk: `encode_directory(chunk)` -> `gzip_compress` -> append to `leaf_blob: Vec<u8>`
   - Creates a root entry per leaf
   - Encodes and compresses root entries
   - Returns `(root_compressed, leaf_blob)`
   - At planet scale: leaf_blob ~20-30 MB, root ~few KB

4. **Drops entries** (line 268) -- frees ~120 MB. **Already implemented (F5).**

5. **Writes header + dirs + leaf blob + tile data** to output file.

**Current peak memory during finalization at planet scale**:
- After dedup drop: ~0 MB from dedup
- During `collect_dir_entries`: ~240 MB transient (raw bytes + parsed entries)
- After `collect_dir_entries`, before `build_directories`: ~120 MB (entries Vec)
- During `build_directories`: ~120 MB (entries) + ~30 MB (leaf_blob) + transients
- After `build_directories`, before entries drop: ~120 MB (entries) + ~30 MB (leaf + root)
- After entries drop: ~30 MB (leaf + root)
- Total peak: **~240 MB** (during `collect_dir_entries` double-buffer)

### 4. Scale Analysis

**How many directory entries at planet scale?**

The estimates project ~5M entries after run-length encoding. This is based on:
- ~100M tiles total (z0-z14 over the planet)
- Ocean-only tiles filtered out by `should_emit` (maybe ~80M tiles are ocean-only)
- ~20M non-ocean tiles emitted
- Dedup reduces some to runs, but at high zoom most are unique
- After RLE: ~5M entries (conservative estimate)

At 24 bytes each: **5M * 24 = 120 MB** for the `Vec<DirEntry>`.
The temp file: **5M * 24 = 120 MB** on disk.
The double-buffer transient: **~240 MB**.

**Assessment**: At ~240 MB peak, this is **not a severe memory problem** for a 64 GB machine where the node store has already been dropped (freeing ~50 GB). The ~240 MB spike occurs during finalization, when the only other significant memory consumers are the sort reader buffers (~20 MB) and OS overhead.

However, the `Vec<DirEntry>` can grow unbounded -- at 24 bytes per entry with potentially 5-10M entries, it can reach 120-240 MB. While manageable on 64 GB, it is the single largest remaining memory allocation during finalization.

### 5. PMTiles v3 Format Constraints

From the PMTiles v3 spec:

1. **Header must be at byte 0** (127 bytes fixed).
2. **Root directory must fit within the first 16,384 bytes** (max 16,257 bytes compressed after the 127-byte header).
3. **Sections can be reordered** -- the only hard constraint is header at position 0 and root directory within the first 16,384 bytes.
4. **Leaf directory offsets are relative** to the `leaf_dirs_offset` field in the header.
5. **Tile data offsets are relative** to the `data_offset` field in the header.

**Critical for streaming approaches**: The spec explicitly allows sections other than the header to be relocated. This means we could write tile data first, then directories. The chicken-and-egg problem (header needs directory offsets, directory offsets need directory sizes, directory sizes need all entries) is the core constraint.

### 6. Feasibility of Streaming Directory Construction

**Question**: Can leaf directories be written incrementally as tiles arrive?

**Analysis**: Yes, with a two-phase file write approach. Here is why:

The `build_leaf_directories` function (line 461) already processes entries in chunks of 4096. Each chunk is independently encoded and compressed. Leaf entries in the root directory reference leaf offsets relative to `leaf_dirs_offset`. If we know the leaf_dirs section starts at a certain file offset, we can compute leaf offsets as we write them.

However, the fundamental problem is that the **header must appear first**, and the header contains `root_dir_offset`, `root_dir_length`, `leaf_dirs_offset`, `leaf_dirs_length`, `data_offset`, `data_length` -- all of which depend on the sizes of sections that appear before the tile data.

**Possible approaches**:

#### Approach A: Seek-back (Write tile data first, then seek to write header)
1. Reserve 16,384 bytes at the start (header + root directory space)
2. Write metadata compressed (small, can estimate)
3. Write leaf directories incrementally as tiles arrive (every 4096 entries, encode and compress a leaf, write it)
4. Write tile data
5. After all tiles are written, build the root directory from the leaf pointers collected during step 3
6. Seek to byte 0, write the 127-byte header with correct offsets
7. Write root directory at byte 127
8. Pad to fill the reserved 16,384 bytes if root is smaller

**Problem**: This requires knowing the metadata and leaf directory sizes before writing tile data. But metadata is fixed (known at start), and leaf directories can be written before tile data. The real problem is we don't know the leaf directory total size until all leaf directories are written, and we need that to compute `data_offset`.

**Revised approach**: Write in order [header_placeholder | root_placeholder | metadata | leaf_dirs | tile_data], then seek back to write the actual header and root.

#### Approach B: Incremental leaf directory writing to temp file (RECOMMENDED)
Instead of materializing all entries in memory, read the streaming `dir_entries.bin` in chunks of 4096 entries, encode each chunk into a leaf directory, write the compressed leaf to a second temp file (`leaf_dirs.bin`), and accumulate only the root entries (one per leaf) in memory. Then build the root directory from the root entries and write the final file.

This avoids the `Vec<DirEntry>` entirely at planet scale. Memory cost:
- Root entries: ~5M / 4096 = ~1220 entries * 24 bytes = ~30 KB
- One 4096-entry chunk in memory at a time: 4096 * 24 = ~98 KB
- Leaf blob in a temp file: ~20-30 MB on disk
- Peak: **< 1 MB** (versus current 240 MB)

#### Approach C: Direct-to-output with seek
Write the final PMTiles file directly without a tile blob temp file:
1. Write placeholder header (127 bytes of zeros)
2. Skip space for root dir (up to ~16 KB)
3. Write metadata
4. As tiles arrive in groups of 4096 entries, encode leaf directories and write them directly to the output file
5. Record the end of leaf directories as `data_offset`
6. Write tile data directly to the output file (no temp file!)
7. After all tiles, build root directory, seek to byte 127, write root
8. Seek to byte 0, write final header

This eliminates BOTH temp files (tile blob and dir entries) but requires `Seek` on the output file.

### 7. Recommended Approach: Approach B (Incremental Leaf Directory Building)

Approach B is the safest and most incremental change. It modifies only the `write_to()` and `collect_dir_entries` methods while preserving the existing file layout.

**Why not Approach C?** While eliminating the tile blob temp file (~3 GB I/O savings) is attractive, it requires:
- Changing the `add_tile` API to write directly to the output file (major refactor)
- Handling the fact that the output file path isn't known during `add_tile` calls
- Risk of corrupted output if the process crashes mid-write (temp file approach is safer)

Approach C should be evaluated separately as a performance optimization (saving 6 GB of I/O at planet scale) rather than as a memory optimization.

### 8. Detailed Implementation Plan

#### Step 1: Fix F4 -- Eliminate double-buffer in `collect_dir_entries` (Quick Win)

**File**: `/home/folk/Programs/elivagar/src/pmtiles_writer.rs`, lines 349-372

Replace `read_to_end` with a `BufReader` that reads 24 bytes at a time:

Currently:
```rust
let mut data = Vec::new();
let mut file = File::open(path)?;
file.read_to_end(&mut data)?;
let mut entries = Vec::with_capacity(num);
let mut pos = 0;
for _ in 0..num {
    let tile_id = read_u64_le(&data, &mut pos);
    // ...
}
```

Should become a `BufReader` reading directly into `Vec<DirEntry>`, parsing 24 bytes at a time via `read_exact`. This halves the transient memory from ~240 MB to ~120 MB.

**Risk**: None. Pure implementation detail.

#### Step 2: Incremental leaf directory building (Core Change)

**File**: `/home/folk/Programs/elivagar/src/pmtiles_writer.rs`

Replace the current flow:
```
collect_dir_entries() -> Vec<DirEntry>   [120 MB in RAM]
build_directories(&entries) -> (root, leaf_blob)
```

With a new flow that never materializes all entries at once:

**New method**: `build_directories_streaming() -> io::Result<(Vec<u8>, Vec<u8>)>` (or with a temp file for leaf blob)

Algorithm:
1. Flush the current run
2. Open the `dir_entries.bin` file for reading with a `BufReader`
3. Read entries in chunks of `LEAF_SIZE` (4096) entries
4. For each chunk:
   - Read 4096 entries (or fewer for the last chunk) from the temp file
   - Call `encode_directory(chunk)` to get raw bytes
   - Call `gzip_compress(&raw)` to get compressed leaf
   - Append compressed leaf to `leaf_blob: Vec<u8>` (or write to a temp file if even ~30 MB is too much)
   - Record a root entry: `DirEntry { tile_id: first_id, offset: leaf_offset, length: leaf_len, run_length: 0 }`
5. After processing all chunks, encode and compress the root entries
6. Return `(root_compressed, leaf_blob)`
7. If total entries <= 16384 (small dataset), read all into a Vec and use the simple root-only path

**Memory profile**:
- One chunk buffer: 4096 * 24 = 98 KB
- Root entries: ~1220 * 24 = ~30 KB
- Leaf blob: ~20-30 MB (accumulated compressed leaf directories)
- **Total: ~30 MB** (versus current 240 MB)

If even the 30 MB leaf blob is a concern, it could be written to a temp file, but 30 MB is negligible at planet scale.

**Integration with `write_to()`**:

Replace lines 239-242 of `write_to()`:
```rust
let entries = self.collect_dir_entries()?;
let (root_bytes, leaf_bytes) = self.build_directories(&entries)?;
```

With:
```rust
let (root_bytes, leaf_bytes, num_entries) = self.build_directories_streaming()?;
```

Remove the separate `entries` Vec and its explicit drop (lines 266-268) since entries are never materialized.

#### Step 3: Handle the small-dataset path

If the total entry count (`self.dir_store.count` or in-memory Vec length) is <= 16384, we need all entries for a single root directory (no leaves). For this case:
- In-memory mode: use `std::mem::take` as before (trivially small)
- Streaming mode with <= 16384 entries: read all entries from the temp file (at most 16384 * 24 = 393 KB -- negligible)

This special case is handled at the top of `build_directories_streaming()`.

#### Step 4: Update `write_to()` to use new flow

The `write_to` method needs to:
1. Drop dedup (already done)
2. Call `build_directories_streaming()` which returns `(root_bytes, leaf_bytes, num_entries)`
3. Build metadata
4. Compute layout
5. Build header with `num_entries`
6. Write output file

The only API change is that `build_directories_streaming` returns the entry count along with the directory bytes, since the caller can no longer call `entries.len()`.

### 9. Memory Cost Summary

| Component | Current | After Step 1 | After Step 2 |
|-----------|---------|-------------|-------------|
| Directory entries Vec | 120 MB | 120 MB | 0 (never materialized) |
| Raw bytes buffer (read_to_end) | 120 MB transient | 0 | 0 |
| Chunk read buffer | 0 | 0 | 98 KB |
| Root entries | (in entries Vec) | (in entries Vec) | 30 KB |
| Leaf blob | 30 MB | 30 MB | 30 MB |
| **Peak finalization** | **~240 MB** | **~150 MB** | **~30 MB** |

### 10. Risk Assessment

**PMTiles format compliance**: Zero risk. The output format is identical -- the same directory structure is produced. Only the build process changes from "all-at-once" to "chunked reading from temp file."

**Reader compatibility**: Zero risk. The PMTiles archive bytes are identical regardless of how the directory was built.

**Correctness**: The chunked approach reads entries in order from the temp file, processes them in LEAF_SIZE chunks (same as the current `entries.chunks(leaf_size)` in `build_leaf_directories`), and produces identical root+leaf bytes. Identical output for identical input.

**Edge cases**:
- Empty archive (0 entries): handled by flush_run + count check
- Very small archive (<= 16384 entries): falls through to simple root-only path
- Exact multiple of LEAF_SIZE entries: last chunk is full, no off-by-one risk
- Single entry: root-only path, trivially correct

**Performance**: The temp file is read sequentially in both approaches. The chunked approach makes more syscalls (reading 4096*24 = 98 KB at a time instead of one bulk read), but the `BufReader` with its buffer absorbs this. The directory encoding and compression are identical. No performance regression expected.

### 11. Broader Context: Is This Worth Doing?

The ~240 MB finalization spike is modest on a 64 GB machine. During finalization, the node store (~50 GB) and way index (~15 GB) have already been dropped. The machine has ample headroom.

**However**, the value proposition is:
1. **Deterministic memory envelope**: The current approach has memory proportional to the number of directory entries (O(n)), while the streaming approach is O(1) regardless of archive size. This eliminates a class of scaling concerns.
2. **Low implementation risk**: The change is localized to `write_to()` and `build_directories`, touching ~50-70 lines. No API changes.
3. **Consistency**: The tile blob is already streamed. Making the directory also streamed completes the "fully streaming" story for the PMTiles writer.
4. **Defense in depth**: If other memory optimizations don't materialize, every 200 MB counts toward the 64 GB budget.

**Recommendation**: Implement Step 1 (F4 fix) immediately as a trivial win. Implement Step 2 when working on the memory experiment matrix, or preemptively as a low-risk improvement. The total effort is about half a day.

### Critical Files

- `/home/folk/Programs/elivagar/src/pmtiles_writer.rs` - Core file containing all directory building logic: `collect_dir_entries()` (line 349), `build_directories()` (line 376), `build_leaf_directories()` (line 461), and `write_to()` (line 235). All modifications happen here.
- `/home/folk/Programs/elivagar/src/pmtiles_writer_tests.rs` - Tests that validate directory encoding, dedup, run-length encoding, and end-to-end `write_to`. Must add a streaming-mode test that exercises the new incremental path.
- `/home/folk/Programs/elivagar/src/pipeline.rs` - Integration point where `PmtilesWriter::new_streaming()` is created (line 1493) and `write_to()` is called (line 1595). No changes needed here, but important for understanding the memory timeline context.
- `/home/folk/Programs/elivagar/examples/bench_pmtiles.rs` - PMTiles benchmark. Should be used to verify no performance regression from the streaming directory change.
