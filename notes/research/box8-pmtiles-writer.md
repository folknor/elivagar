# Box 8 Deep Investigation: PMTiles Writer and Finalization

Date: 2026-02-28
Primary file: `src/pmtiles_writer.rs` (730 lines)
Benchmark file: `examples/bench_pmtiles.rs` (252 lines)

---

## 1. Hilbert Tile ID System

### Implementation

The Hilbert curve maps 2D tile coordinates (z, x, y) to a 1D tile ID for spatial clustering
in the PMTiles v3 format. The implementation lives in three functions:

- `xy_to_tile_id(z, x, y) -> u64` (`pmtiles_writer.rs:632-641`): Forward mapping.
  Computes a zoom-level base offset `(4^z - 1) / 3` (the cumulative count of all tiles at
  zoom levels 0..z-1), then adds the Hilbert distance `hilbert_xy2d(n, x, y)`.

- `tile_id_to_zxy(tile_id) -> (u8, u32, u32)` (`pmtiles_writer.rs:644-671`): Reverse mapping.
  Searches for the zoom level by iterating z from 0 upward until `tile_id < base(z+1)`,
  then calls `hilbert_d2xy(n, d)` to recover (x, y).

- `hilbert_xy2d(n, x, y) -> u64` (`pmtiles_writer.rs:674-686`): Standard iterative Hilbert
  curve algorithm. Processes bits from MSB to LSB, accumulating the distance `d` and applying
  rotations via `hilbert_rot()`.

- `hilbert_d2xy(n, d) -> (u32, u32)` (`pmtiles_writer.rs:688-710`): Inverse of xy2d.

- `hilbert_rot(n, x, y, rx, ry)` (`pmtiles_writer.rs:712-720`): Rotation/reflection helper.
  Swaps x/y and conditionally mirrors based on quadrant.

### Computational cost

Each `xy_to_tile_id` call does `z` iterations of the inner loop (at most 14 for max_zoom=14),
each iteration involving a few bitwise ops, a multiply, and a conditional swap. This is O(z)
per tile, with z <= 14.

Each `tile_id_to_zxy` call has an additional linear search for the zoom level (up to 14
iterations), plus the O(z) `hilbert_d2xy`. Total: O(z) per tile.

### Where it appears in the hot path

The Hilbert computation appears in two distinct hot paths:

1. **Emission phase** (PBF processing, `pipeline.rs:665,1053,1106,1126,1192,1290`):
   `xy_to_tile_id` is called for every (tile, zoom, feature) combination to build the sort
   key. At planet scale with ~500M+ features and multi-tile fanout, this amounts to billions
   of calls. However, the cost per call is ~20 integer operations -- trivially cheap compared
   to the clipping, simplification, and MVT encoding that surrounds each call.

2. **Writer thread** (`pipeline.rs:1401`): `tile_id_to_zxy` is called to convert back from
   tile_id to (z,x,y) for the `add_tile` API. Then `add_tile` (`pmtiles_writer.rs:188`)
   immediately calls `xy_to_tile_id` again to recompute tile_id. This is a redundant
   round-trip: the pipeline already HAS the tile_id, but the writer's public API takes
   (z,x,y). The redundancy is ~28 integer ops per tile -- trivial at Denmark scale (54K tiles),
   negligible even at planet scale (millions of tiles), since the writer thread is not the
   bottleneck.

   **Minor optimization opportunity**: Add an `add_tile_by_id(tile_id, data)` method to skip
   the double conversion. Low priority -- the writer thread is I/O-bound and this saves
   nanoseconds per tile.

### Hilbert ordering and I/O effects

Hilbert ordering means spatially nearby tiles are close in the blob file. This has two benefits:
- **Reader locality**: A map viewer panning across a region reads tiles with adjacent Hilbert
  IDs, which map to sequential offsets in the blob, enabling good sequential/prefetch I/O.
- **Dedup clustering**: Ocean tiles covering adjacent spatial regions arrive consecutively,
  maximizing run-length encoding opportunities (consecutive identical tiles become a single
  directory entry with run_length > 1).

---

## 2. Tile Deduplication

### Hash function

`DefaultHasher` is used (`pmtiles_writer.rs:190`), which is `SipHash-1-3` in the Rust
standard library (as of Rust 1.36+, the stdlib uses SipHash-1-3 for `DefaultHasher`). The
comment at line 196 correctly identifies it as SipHash-1-3.

**What is hashed**: The compressed (gzipped) tile blob (`data: &[u8]`) is hashed via the
`Hash` trait implementation for `[u8]`. This means dedup operates on the already-compressed
data. This is correct -- since tiles are gzipped before reaching the writer, two tiles with
identical uncompressed content will produce identical compressed output (same compressor,
same level, deterministic), so hashing compressed data is equivalent to hashing uncompressed
data.

**Important**: If compression were non-deterministic (e.g., different compressor instances
with different state), identical source data could hash differently. The pipeline uses
per-rayon-thread `Compressor` instances via `map_init()` (`pipeline.rs:1460`), and
`libdeflater`'s gzip output is deterministic for the same input at the same level, so this
is safe.

### Data structure

```rust
dedup: HashMap<u64, (u64, u32)>,  // pmtiles_writer.rs:125
```

- **Key**: `u64` -- the SipHash-1-3 hash of the compressed tile data.
- **Value**: `(u64, u32)` -- `(offset_in_blob, compressed_length)`.
- Total per-entry memory: key (8 bytes) + value (12 bytes) + HashMap overhead (control byte
  + padding) = ~32-56 bytes per entry depending on HashMap load factor and implementation.
  The standard `HashMap` (hashbrown) uses 1 byte of control metadata per slot plus alignment.
  At 87.5% load factor, effective cost is roughly 24 bytes payload + ~4 bytes control per
  entry, so ~28-32 bytes per entry on average.

### Dedup cap

```rust
const MAX_DEDUP_ENTRIES: usize = 1_000_000;  // pmtiles_writer.rs:56
```

The comment explains the rationale: "At planet scale, unlimited dedup grows to ~7 GB.
Capping at 1M entries keeps the map under ~50 MB while still deduplicating the ocean fill
tiles (which are added early and remain cached)."

**Memory calculation**: 1M entries * ~32 bytes effective = ~32 MB for the HashMap data,
plus hashbrown's power-of-2 sizing overhead. The comment says ~50 MB, which accounts for
hashbrown allocating the next power of 2 above 1M (~2M slots) * ~24 bytes = ~48 MB. This
is consistent.

### Behavior when cap is reached

```rust
if self.dedup.len() < MAX_DEDUP_ENTRIES {
    self.dedup.insert(hash, (offset, length));
}
// pmtiles_writer.rs:220-222
```

When the cap is reached, the writer **stops inserting new entries** but continues checking
existing entries. This means:
- Early tiles (ocean, low-zoom) remain in the map and continue to be deduplicated.
- Late tiles (high-zoom, high-detail) that are unique will be written even if they have
  content identical to another late tile that also wasn't cached.
- There is **no eviction** -- it's a hard cap with no LRU or other replacement policy.

### Collision guard

```rust
if dup_length == data.len() as u32 {  // pmtiles_writer.rs:200
```

After a hash match, the code also checks that the compressed length matches. The comment
notes this gives ~2^-81 false dedup probability per pair (2^-64 for hash collision AND
2^-17 for matching length). No full content comparison is performed -- that would require
seeking back in the blob file or storing the original data.

### Expected dedup ratios

**Denmark** (54K unique tiles from memory context): At Denmark scale, the dedup map will
hold at most 54K entries (well under the 1M cap), so all tiles are eligible for dedup.
The dedup ratio depends on how many tiles share identical content -- primarily ocean-only
tiles, which are skipped by the `should_emit` filter in the reader thread
(`pipeline.rs:1356`). After ocean-only filtering, dedup opportunities mainly come from
tiles where the visible features are identical (same POIs, same roads at a zoom level).
This is likely modest -- perhaps 5-20% dedup ratio.

**Planet** (millions of tiles): The 1M cap will be hit early. Ocean tiles are already
excluded by `should_emit`, so dedup targets identical land tiles. At planet scale with
diverse content, the dedup ratio for non-ocean tiles is likely low (< 5%). The cap
means tiles added after the first 1M unique tiles get zero dedup opportunity. However,
since most dedup-worthy tiles (empty/sparse tiles at low zooms) are processed early due
to Hilbert ordering, the cap's impact is mitigated.

### Could a bloom filter work?

A bloom filter could replace the HashMap for space efficiency, but it would only answer
"possibly seen" / "definitely not seen" -- it cannot store the (offset, length) needed to
point the duplicate entry at the original blob. You'd need the bloom filter PLUS a way to
look up the original offset. Possible approach: bloom filter as a pre-screen, then a
smaller bounded HashMap for confirmed matches. But this adds complexity for marginal gain
since the current approach is already capped at ~50 MB.

A more interesting alternative: **sorted-file dedup** -- since tiles arrive in Hilbert
order, spatially nearby tiles are adjacent. A small window (e.g., last 100-1000 tiles)
could catch local duplicates without a global map. This would use fixed ~100KB memory and
catch the most likely duplicates (adjacent tiles at the same zoom with similar/identical
content). But it would miss cross-zoom duplicates entirely.

---

## 3. Streaming Architecture

### Two modes

The writer supports two storage backends, selected at construction:

1. **In-memory** (`PmtilesWriter::new`, `pmtiles_writer.rs:144`):
   - Tile blob: `TileBlob::Memory(Vec<u8>)` -- all compressed tiles appended to a single Vec.
   - Directory entries: `DirStore::Memory(Vec<DirEntry>)` -- all entries in a Vec.
   - Best for small extracts and tests. Used by the benchmark (`bench_pmtiles.rs:193`).

2. **Streaming** (`PmtilesWriter::new_streaming`, `pmtiles_writer.rs:160`):
   - Tile blob: `TileBlob::File { writer, path, offset }` -- tiles written to `tiles.blob`
     in `tmp_dir` via a `BufWriter` with 1 MB buffer (`pmtiles_writer.rs:163`).
   - Directory entries: `DirStore::Streaming { writer, path, count }` -- entries serialized
     to `dir_entries.bin` via `BufWriter` with **default buffer size** (8 KB, Rust std
     default, `pmtiles_writer.rs:167` uses `BufWriter::new(dir_file)` without specifying
     capacity).
   - Required for planet-scale to avoid multi-GB RAM usage for the blob and directory.

### Blob file layout

Sequential append of compressed tile data. Each `add_tile` call:
1. Records the current `offset` (file position).
2. Writes `data` (the gzipped MVT tile) via `writer.write_all(data)`.
3. Advances `offset` by `data.len()`.

The blob is written once, sequentially. No seeking, no random access during the add phase.
At planet scale (~3 GB of compressed tiles per the comment at line 87), this produces a
single ~3 GB temp file.

### Directory temp file layout

Each `DirEntry` is serialized as 24 bytes of fixed-width little-endian fields
(`pmtiles_writer.rs:311-314`):
```
tile_id:    u64 (8 bytes LE)
offset:     u64 (8 bytes LE)
length:     u32 (4 bytes LE)
run_length: u32 (4 bytes LE)
```

The `DirEntry` struct is validated as 24 bytes at compile time (`pmtiles_writer.rs:69`):
```rust
const _: () = assert!(std::mem::size_of::<DirEntry>() == 24);
```

Entries are NOT written per-tile. They are written per-run, after run-length encoding.
The `push_dir_entry` method (`pmtiles_writer.rs:323-336`) extends the current run if the
new tile has consecutive tile_id, same offset, and same length. Otherwise it flushes the
old run to disk and starts a new one. This compaction is significant: for ocean-fill tiles
and other dedup'd runs, potentially thousands of tiles collapse to a single directory entry.

### Buffer size concern

The directory temp file uses `BufWriter::new()` (line 167), which defaults to 8 KB. Each
entry is 24 bytes, so the buffer holds ~341 entries before flushing. This is adequate for
the write pattern (sequential, moderate frequency), but could be increased to 64-256 KB
for marginally fewer syscalls at planet scale.

The blob temp file uses `BufWriter::with_capacity(1 << 20, ...)` (line 163) = 1 MB buffer.
This is appropriate for the higher data volume.

**Asymmetry**: The blob gets a 1 MB buffer while the directory gets 8 KB. The directory
writes are much smaller per call (24 bytes vs. kilobytes), but at planet scale with millions
of runs, a larger buffer would reduce syscall overhead. Not a bottleneck, but a consistency
issue.

### Filesystem interaction

Both temp files are created in `config.tmp_dir` (typically `data/tilegen_tmp/`). They are:
- Created fresh at the start (`File::create` truncates).
- Written sequentially during the assemble phase.
- Read back during finalization (blob via `io::copy`, directory via `read_to_end`).
- Deleted after read-back (`std::fs::remove_file`).

No `fsync` is called at any point. This is correct for temp files -- if the process crashes,
the output is incomplete anyway, and the temp files are in a known temp directory that gets
cleaned up by `dev clean`.

---

## 4. Finalization (`write_to`)

### Step-by-step walkthrough (`pmtiles_writer.rs:232-297`)

**Step 1: Collect directory entries** (line 233)
```rust
let entries = self.collect_dir_entries()?;
```
Calls `flush_run()` to emit the final pending run, then:
- In-memory mode: `std::mem::take(entries)` -- O(1), steals the Vec.
- Streaming mode: `writer.flush()`, then `File::open(path)` + `file.read_to_end(&mut data)`,
  then parses the binary format back into `Vec<DirEntry>`.

**Critical**: In streaming mode, the entire directory is loaded into memory as a
`Vec<DirEntry>`. At 24 bytes per entry, this is `num_runs * 24` bytes. For Denmark with
~54K tiles, if dedup collapses many runs, this might be ~30-50K entries = ~720 KB - 1.2 MB.
For planet scale, the question is how many distinct runs exist after run-length encoding.

Planet estimate: If there are ~5M unique tiles (after ocean filtering and dedup) with low
dedup rates, most tiles form their own run (run_length=1). So ~5M entries * 24 bytes = ~120 MB.
This is the first memory spike during finalization.

**Step 2: Build metadata JSON** (line 234)
```rust
let metadata_json = build_metadata(&self.config);
```
Generates a JSON string listing all 26 Shortbread layers. Small, cold path. A few KB.

**Step 3: Build directories** (line 236)
```rust
let (root_bytes, leaf_bytes) = self.build_directories(&entries)?;
```
This is the core of directory encoding. See Section 5 for details.

**Step 4: Compress metadata** (line 237)
```rust
let metadata_compressed = gzip_compress(metadata_json.as_bytes())?;
```
Creates a `Compressor`, allocates a bound-sized buffer, compresses, truncates. Small data.

**Step 5: Clean up directory temp file** (lines 241-243)
```rust
if let DirStore::Streaming { path: dir_path, .. } = &self.dir_store {
    drop(std::fs::remove_file(dir_path));
}
```
Best-effort deletion. The `drop()` discards the `Result`. The `writer` BufWriter from
`DirStore::Streaming` is not explicitly dropped or flushed here, but it was already
flushed inside `collect_dir_entries()` (line 345). However, the `File` inside the
`BufWriter` is still open. The `remove_file` unlinks the inode; the file is truly deleted
when the `BufWriter` (and its inner `File`) is dropped (which happens when `PmtilesWriter`
is dropped).

**Step 6: Compute layout** (lines 246-258)
```
[header: 127 bytes] [root_dir] [metadata] [leaf_dirs] [tile_data]
```
All offsets and lengths are computed from the in-memory byte vectors.

**Step 7: Build header** (lines 261-271)
127-byte fixed header. Pure computation, no I/O.

**Step 8: Write output file** (lines 273-296)
Opens the output file with `BufWriter::new(file)` (default 8 KB buffer, line 274).
Writes:
1. Header (127 bytes)
2. Root directory (compressed, typically a few KB to tens of KB)
3. Metadata (compressed, a few hundred bytes)
4. Leaf directories (compressed, variable -- see Section 5)
5. Tile data:
   - In-memory: `w.write_all(vec)` -- single large write from the Vec.
   - Streaming: Flush the blob BufWriter, reopen the blob file for reading with a 1 MB
     BufReader, `io::copy(&mut reader, &mut w)`, then delete the blob file.

**Step 9: Final flush** (line 295)
```rust
w.flush()?;
```

### Peak memory during finalization

Simultaneously alive during `write_to`:
1. `entries: Vec<DirEntry>` -- all directory entries (planet: ~120 MB estimated)
2. Root directory encoded + compressed (a few KB to ~100 KB)
3. Leaf directory encoded + all compressed leaf blobs concatenated (planet: see Section 5)
4. Metadata compressed (< 1 KB)
5. `dedup: HashMap<u64, (u64, u32)>` -- still alive, up to ~50 MB at cap
6. Output file BufWriter (8 KB default buffer)

For planet scale, the dominant costs are:
- The `entries` Vec: ~120 MB
- The leaf blob: depends on directory size (see Section 5)
- The dedup HashMap: ~50 MB

The `entries` Vec is consumed by `build_directories` which iterates over it in chunks, but
the Vec itself stays alive until `write_to` returns (it's a local variable in `write_to`).
The leaf blob is built by `build_leaf_directories` and returned as a `Vec<u8>`.

**Optimization opportunity**: The `entries` Vec could be dropped after `build_directories`
returns, before the write phase begins. Currently it's kept alive unnecessarily. Wrapping
the directory build in a block or explicitly dropping `entries` after line 236 would free
~120 MB before the write phase.

Similarly, the `dedup` HashMap is no longer needed during finalization but stays alive until
the `PmtilesWriter` is dropped. It could be cleared with `self.dedup.clear(); self.dedup.shrink_to_fit();` at the start of `write_to` to reclaim ~50 MB.

### Second-pass I/O cost

In streaming mode, finalization involves reading back:
1. The directory temp file: `num_runs * 24` bytes (planet: ~120 MB)
2. The tile blob: `total_compressed_bytes` (planet: ~3 GB)

Both are sequential reads. The directory file was recently written so likely in page cache.
The blob file is ~3 GB, which may or may not be in page cache depending on available RAM.
On a 64 GB machine, the blob should be in cache. On constrained machines, this is a full
re-read from disk.

The `io::copy` for the blob uses a BufReader (1 MB) and BufWriter (8 KB default). The
BufWriter default is undersized for this volume -- a 1 MB BufWriter would be more
appropriate for multi-GB sequential writes.

**Verified concern from theoretical review**: "Finalization re-reads streamed directory and
tile blob — second-pass I/O." Confirmed. The blob re-read is unavoidable because the
PMTiles format requires the header (which contains offsets) to appear first, before the
tile data. The directory re-read could be avoided if entries were kept in memory, but that
defeats the purpose of streaming mode.

---

## 5. Directory Encoding

### PMTiles v3 directory format

The directory uses a **columnar varint format** (`encode_directory`, `pmtiles_writer.rs:491-515`):

```
[count: varint]
[column 1: delta-encoded tile IDs, count varints]
[column 2: run lengths, count varints]
[column 3: lengths, count varints]
[column 4: offsets with contiguity optimization, count varints]
```

**Column 1 - Tile IDs** (`encode_tile_id_column`, line 518-525): Delta-encoded. Each entry
stores the difference from the previous tile ID. Since tiles arrive in sorted Hilbert order,
deltas are positive and often small (especially at high zoom), encoding efficiently as 1-2
byte varints.

**Column 2 - Run lengths** (lines 502-504): Raw varint per entry. Usually 1 (most tiles are
unique), but can be large for dedup'd regions (e.g., ocean fill runs).

**Column 3 - Lengths** (lines 507-509): Compressed tile size as varint. Typically 1-3 bytes
per entry (tile sizes in hundreds to thousands of bytes).

**Column 4 - Offsets** (`encode_offset_column`, line 528-542): Contiguity-optimized. If
entry i's offset equals `entries[i-1].offset + entries[i-1].length` (i.e., tiles are
contiguous in the blob), encodes as 0. Otherwise encodes as `offset + 1`. In a clustered
archive, most tiles ARE contiguous, so most offsets encode as a single byte (0).

### Root vs. leaf directory split

`build_directories` (`pmtiles_writer.rs:366-377`):

```rust
const MAX_ROOT_ENTRIES: usize = 16384;
const LEAF_SIZE: usize = 4096;
```

- If `entries.len() <= 16384`: All entries go into the root directory. No leaf directories.
  Denmark with ~54K tiles might have ~30-50K entries after run-length encoding -- this exceeds
  16384, so Denmark likely uses leaf directories.

  Wait, let me reconsider. Run-length encoding in `push_dir_entry` merges consecutive tiles
  with the same data. For Denmark, many tiles are unique, so most entries have run_length=1.
  With ~54K tiles and perhaps 5-20% dedup, there might be ~45-50K entries. This exceeds
  MAX_ROOT_ENTRIES=16384, so leaf directories are used.

- If entries exceed 16384: `build_leaf_directories` (`pmtiles_writer.rs:450-484`) is called.

### Leaf directory construction

`build_leaf_directories` (`pmtiles_writer.rs:450-484`):

1. Entries are split into chunks of LEAF_SIZE=4096 entries each.
2. Each chunk is encoded with `encode_directory()`, then gzip-compressed.
3. The compressed leaf is appended to a `leaf_blob: Vec<u8>`.
4. A root entry is created pointing to the leaf's offset and length in the leaf blob.
5. The root entries (one per leaf) are encoded and compressed as the root directory.

For Denmark (~45K entries): ~11 leaf chunks, ~11 root entries. Each leaf is ~4096 entries *
~6-10 bytes per entry (varints) = ~25-40 KB raw, compressing to maybe ~15-25 KB. Total leaf
blob: ~165-275 KB. Root directory: 11 entries, trivially small.

For planet (~5M entries): ~1220 leaf chunks, ~1220 root entries. Each leaf: ~25-40 KB raw,
compressing to ~15-25 KB. Total leaf blob: ~18-30 MB. Root directory: 1220 entries, a few KB
compressed.

### Memory during directory encoding

The `encode_directory` function allocates a `Vec<u8>` per directory chunk. For each of the
~1220 planet-scale leaf chunks, it creates a ~25-40 KB Vec, compresses it (another ~25 KB
allocation), then extends the `leaf_blob` Vec. The per-leaf Vecs are freed after each
iteration. The `leaf_blob` grows to ~18-30 MB.

Peak memory for directory encoding at planet scale:
- `entries` slice: still alive (~120 MB, owned by caller)
- `leaf_blob: Vec<u8>`: ~30 MB
- `root_entries: Vec<DirEntry>`: ~1220 * 24 = ~29 KB
- Per-chunk transient allocations: ~100 KB

**Verified concern from theoretical review**: "Directory encoding is all-at-once in memory."
Confirmed, but the actual memory impact is moderate (~30 MB for leaf blob at planet scale).
The main memory pressure is the `entries` Vec (~120 MB), not the directory encoding itself.

### Compression of directories

Both root and leaf directories are gzip-compressed using `gzip_compress()` (`pmtiles_writer.rs:549-558`).
This function creates a new `Compressor` each time with `CompressionLvl::default()` (level 6).
At ~1220 calls for planet-scale leaf directories, the compressor allocation is repeated each
time.

**Minor optimization**: Reuse a single `Compressor` across leaf compressions. The `Compressor`
is a libdeflater object -- creating and destroying it ~1220 times wastes allocation cycles.
Pass a mutable compressor reference into `build_leaf_directories` and reuse it.

---

## 6. What the Review Might Have Missed

### 6.1. Redundant Hilbert conversion in writer thread

As noted in Section 1, the pipeline has the tile_id already (`pipeline.rs:1401`), converts it
to (z,x,y) via `tile_id_to_zxy`, passes it to `add_tile`, which immediately calls
`xy_to_tile_id` to get back the same tile_id. This is wasteful but trivially cheap. An
`add_tile_by_id(tile_id: u64, data: &[u8])` method would eliminate this. Negligible impact.

### 6.2. Output BufWriter uses default buffer size

`write_to` opens the output file with `BufWriter::new(file)` (line 274), giving an 8 KB
buffer. For the tile data write phase (multi-GB `io::copy` in streaming mode), this means
frequent small writes to the OS. The blob BufReader uses 1 MB (`BufReader::with_capacity(1 << 20, ...)`
at line 288), but the output BufWriter is only 8 KB. This asymmetry means the `io::copy`
reads 1 MB chunks but writes them in 8 KB pieces.

**Recommendation**: Use `BufWriter::with_capacity(1 << 20, file)` for the output file.

### 6.3. `entries` Vec lifetime extends too long

In `write_to`, the `entries` Vec is allocated at line 233 and lives until the function
returns at line 296. It's only needed through line 236 (`build_directories`). After that, it
holds ~120 MB of dead data while the write phase runs. Dropping it early (or scoping it)
would reduce peak memory during the write phase.

```rust
// Current: entries lives through the entire function
let entries = self.collect_dir_entries()?;
let (root_bytes, leaf_bytes) = self.build_directories(&entries)?;
// entries is dead here but not dropped until function end
```

### 6.4. `dedup` HashMap not cleared during finalization

The dedup HashMap (~50 MB at cap) is no longer needed after all tiles are added. It could be
cleared at the start of `write_to()` to free ~50 MB before the directory + write phase.

### 6.5. No `write_to` with pre-existing file handle

The `write_to` method takes a `&Path` and creates the file internally. There's no way to
pass a pre-opened file descriptor or use a pre-allocated file (e.g., `fallocate` for
avoiding fragmentation on the output). Low priority but relevant for planet-scale output
where the file will be ~100+ GB.

### 6.6. Leaf directory compression creates a new Compressor per leaf

`gzip_compress()` at `pmtiles_writer.rs:549-558` allocates a new `Compressor` for each call.
During `build_leaf_directories`, it's called once per leaf chunk (~1220 times for planet).
Each call allocates internal compression state. The function could accept a `&mut Compressor`
parameter for reuse.

### 6.7. No parallelism in finalization

The `write_to` method is entirely single-threaded:
- Directory collection: sequential read
- Directory encoding: sequential iteration over chunks
- Directory compression: sequential per-chunk
- Output write: sequential

For planet scale, the directory encoding + compression of ~1220 leaf chunks could
potentially be parallelized with rayon. Each chunk is independent. However, the leaf blob
is built by sequential appending, which would need to be changed to a parallel collect +
concatenate. The benefit is modest (directory encoding is not the bottleneck -- the blob
copy is).

### 6.8. Blob copy dominates finalization time

For planet scale (~3 GB blob), the `io::copy` in streaming mode reads 3 GB from the temp
file and writes 3 GB to the output file. This is ~6 GB of sequential I/O. On a typical SSD
at ~500 MB/s sequential, this takes ~12 seconds. On NVMe at ~3 GB/s, it's ~2 seconds.

The blob copy is unavoidable in the current architecture because the PMTiles format requires
the header + directories to appear before the tile data, and the header contains the data
offset, which depends on the directory sizes, which aren't known until all tiles are added.

**Alternative**: Pre-allocate the header + estimated directory space, write tile data at a
known offset, then fill in the header + directories at the end with seeks. This would
eliminate the temp file and second-pass I/O entirely. However, this requires:
1. Estimating the directory size upfront (or reserving a generous max).
2. Using `seek` + `write` on the output file (not streaming).
3. Risk of underestimating directory size (would need to shift the entire blob).

A safer variant: write tile data to the final output file starting at a generous offset
(e.g., 10 MB for header+dirs), then after finalization, if the actual header+dirs are
smaller, either pad or use the PMTiles leaf directory mechanism to skip unused space.
This is complex but would eliminate the entire blob temp file.

### 6.9. Partial write / crash resilience

There is no crash resilience. If the process is killed during `write_to`:
- The output file may be partially written and corrupt.
- The temp files (blob, dir) may remain on disk in `tilegen_tmp/`.

The `dev clean` command handles temp file cleanup. The output file should be checked for
completeness after a run (e.g., verify the header is present and tile count matches).
This is not a performance concern but a correctness note.

### 6.10. Run-length encoding effectiveness

The run-length encoding in `push_dir_entry` (`pmtiles_writer.rs:323-336`) only extends a
run if three conditions are met simultaneously:
1. `tile_id == next_id` (consecutive Hilbert ID)
2. `offset == run.offset` (same blob offset -- implies dedup'd to same data)
3. `length == run.length` (same compressed size)

This means runs only form for consecutive tiles that are deduped to the exact same blob.
This is most effective for ocean fill tiles (large regions of identical water tiles) and
empty/sparse tiles at low zoom levels.

For non-deduped tiles, every tile is its own run (run_length=1), so the directory entry
count equals the tile count. This is the common case at high zoom levels.

### 6.11. `collect_dir_entries` reads entire file at once

In streaming mode, `collect_dir_entries` (`pmtiles_writer.rs:340-363`) calls
`file.read_to_end(&mut data)` (line 350), reading the entire directory temp file into a
`Vec<u8>`. Then it parses entries from this buffer into another `Vec<DirEntry>`.

Briefly, two copies of the directory data exist simultaneously:
- `data: Vec<u8>` -- raw bytes (~120 MB at planet scale)
- `entries: Vec<DirEntry>` -- parsed structs (~120 MB at planet scale)

Total: ~240 MB for directory collection. The `data` Vec is dropped at the end of the
match arm, before `write_to` proceeds, so this is a transient spike.

**Optimization**: Instead of `read_to_end`, read in chunks and parse on the fly, building
the `entries` Vec directly without the intermediate `Vec<u8>`. This halves the transient
memory for directory collection.

```rust
// Alternative: read entries directly
let mut file = BufReader::new(File::open(path)?);
let mut entries = Vec::with_capacity(num);
let mut buf = [0u8; 24]; // One entry at a time
for _ in 0..num {
    file.read_exact(&mut buf)?;
    entries.push(DirEntry {
        tile_id: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
        offset: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
        length: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
        run_length: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
    });
}
```

---

## 7. Scale Projections

### Denmark (baseline: 54K tiles, 286 MB output)

| Component | Estimated size |
|-----------|---------------|
| Dedup HashMap | ~2 MB (54K entries * ~32 bytes) |
| Directory entries | ~45K entries * 24 bytes = ~1 MB |
| Leaf blob | ~200 KB compressed |
| Blob temp file | 286 MB |
| Peak finalization memory | ~5 MB (dirs) + ~50 MB (dedup) + blob copy |

At Denmark scale, the writer is not a bottleneck. Finalization is fast (< 1 second).

### Planet (target: ~5M tiles, ~3 GB output, 64 GB RAM)

| Component | Estimated size |
|-----------|---------------|
| Dedup HashMap | ~50 MB (capped at 1M entries) |
| Directory entries (Vec) | ~5M * 24 bytes = ~120 MB |
| Directory entries read buffer | ~120 MB (transient, from read_to_end) |
| Leaf blob (compressed) | ~20-30 MB |
| Blob temp file | ~3 GB on disk |
| Output BufWriter | 8 KB (should be 1 MB) |
| Peak finalization memory | ~240 MB (dir collect) + ~50 MB (dedup) + ~30 MB (leaf blob) = ~320 MB |

The ~320 MB peak finalization memory is comfortable on a 64 GB machine. The main cost is
the blob re-copy I/O (~3 GB read + ~3 GB write = ~6 GB sequential I/O).

### Worst case: ultra-dense planet with minimal dedup

If tile count reaches ~10M (very dense metro areas at z14 with many layers):
- Directory entries: ~10M * 24 = ~240 MB
- Read buffer transient: ~240 MB
- Peak: ~530 MB + dedup + leaf blob = ~620 MB
- Still well within 64 GB

---

## 8. Summary of Actionable Findings

### High priority

1. **Output BufWriter undersized** (`pmtiles_writer.rs:274`): The output file uses an 8 KB
   BufWriter for writing multi-GB data. Change to `BufWriter::with_capacity(1 << 20, file)`.
   Zero risk, immediate I/O improvement for streaming mode.

2. **`entries` Vec lifetime too long** (`pmtiles_writer.rs:233`): The Vec holds ~120 MB at
   planet scale through the entire write phase when it's only needed through directory
   encoding. Drop it after `build_directories` returns.

3. **`dedup` HashMap not cleared** (`pmtiles_writer.rs:125`): The HashMap holds ~50 MB and
   is not needed during finalization. Clear it at the start of `write_to`.

### Medium priority

4. **`collect_dir_entries` double-buffers** (`pmtiles_writer.rs:348-350`): The `read_to_end`
   approach creates a transient ~240 MB spike (raw bytes + parsed entries simultaneously).
   Read entries directly from the file in 24-byte chunks.

5. **Directory temp file BufWriter uses 8 KB default** (`pmtiles_writer.rs:167`): Increase
   to `BufWriter::with_capacity(1 << 16, dir_file)` (64 KB) for fewer syscalls.

6. **`gzip_compress` allocates new Compressor per call** (`pmtiles_writer.rs:549-558`):
   During leaf directory construction, this is called ~1220 times at planet scale. Pass a
   reusable compressor.

### Low priority

7. **Redundant tile_id <-> (z,x,y) conversion**: Add `add_tile_by_id` method to avoid
   the pointless round-trip. Negligible performance impact but cleaner API.

8. **Dedup cap behavior**: The hard cap with no eviction is pragmatic. Alternative
   strategies (windowed, LRU, tiered) add complexity for minimal gain given that most
   dedup opportunities are early (ocean, low-zoom) and captured within the first 1M entries.
   Monitor dedup hit rate in production before redesigning.

9. **Parallel leaf directory compression**: Could parallelize the ~1220 leaf compressions
   with rayon. Low impact since directory encoding is fast relative to blob I/O.

### Verified claims from theoretical review

| Claim | Verdict |
|-------|---------|
| "Dedup cap protects RAM but can miss late duplicate opportunities" | **Confirmed**. Hard cap at 1M entries, no eviction. Late duplicates are missed. Impact likely low for non-ocean tiles. |
| "Finalization re-reads streamed directory and tile blob" | **Confirmed**. Both are sequential re-reads. Blob re-read (~3 GB at planet) dominates. Directory re-read (~120 MB) is modest. |
| "Directory encoding is all-at-once in memory; entry volume creates transient memory spikes" | **Partially confirmed**. The spike comes from `collect_dir_entries` double-buffering (raw bytes + parsed entries), not from the encoding itself. The leaf blob encoding is chunked and modest (~30 MB). |
