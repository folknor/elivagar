# P4: Configurable Sort Chunk Memory Profile

## Status: IMPLEMENTED (`b82afae`)

All steps complete:
- Renamed `SORT_CHUNK_SIZE` → `DEFAULT_SORT_CHUNK_SIZE`, added `sort_chunk_size` to `TilegenConfig`
- Added `--sort-budget <size>` CLI flag with `parse_byte_size()` (accepts `256M`, `1G`, raw bytes; minimum 64 MB)
- Fixed ocean byte accounting (`+8` → `+sizeof::<SortRecord>()`)
- Updated lib.rs doc example and CLAUDE.md CLI flags section

## Detailed Analysis and Implementation Plan

### 1. Current State: How Sort Chunk Memory Works

#### 1.1 The Budget Constant

The sort chunk budget is a single constant at `/home/folk/Programs/elivagar/src/pipeline.rs:117`:

```rust
const SORT_CHUNK_SIZE: usize = 1 << 30; // 1 GB
```

This constant is used in exactly two places:
- **Line 335**: `SortWriter::new(&config.tmp_dir.join(SORT_CHUNKS_DIR), SORT_CHUNK_SIZE)?` -- creating a fresh sort writer during full runs
- **Line 188**: `sort::SortWriter::resume(&config.tmp_dir.join(SORT_CHUNKS_DIR), SORT_CHUNK_SIZE, pbf_chunks)?` -- resuming from checkpoint for `--skip-to ocean`

#### 1.2 How SortWriter Tracks Memory

In `/home/folk/Programs/elivagar/src/sort.rs`, the `SortWriter` struct tracks:
- `buffer: Vec<SortRecord>` -- the in-memory accumulation buffer
- `buffer_bytes: usize` -- running byte count
- `chunk_size_bytes: usize` -- the threshold from the constant

The memory tracking in `push()` (line 137):
```rust
self.buffer_bytes += record.data.len() + std::mem::size_of::<SortRecord>(); // data.len() + 32
```

This counts `data.len() + 32` per record (32 = `sizeof(SortRecord)` = 8-byte key + 24-byte Vec header). Since `encode_feature_data_with_attrs` uses exact `Vec::with_capacity`, there is no over-allocation, so this accounting is accurate for the in-memory footprint. The flush trigger at line 139 fires when `buffer_bytes >= chunk_size_bytes`.

After the first chunk is flushed, `buffer.clear()` retains the `Vec<SortRecord>` capacity, so subsequent chunks reuse the same allocation without Vec growth cost.

#### 1.3 Ocean Processing: Same Budget, Different Path

Ocean processing (`/home/folk/Programs/elivagar/src/ocean.rs:278`) reads the chunk budget from the sort writer:
```rust
let chunk_size = sort_writer.chunk_size_bytes();
```

Each rayon worker in the ocean fold accumulates records in a thread-local `OceanAcc` and flushes when `acc.bytes >= chunk_size`. However, the ocean byte accounting uses `r.data.len() + 8` (line 318), not `+ 32` like the sort writer -- it undercounts by 24 bytes per record. This means ocean chunks run slightly over budget, but the difference is small (~3% at typical record sizes).

#### 1.4 What a SortRecord Contains

From `/home/folk/Programs/elivagar/src/wire_format.rs`, a typical record payload is:
- **Points**: ~50-100 bytes (8 osm_id + 1 geom_type + 2 cmd_count + 12 geom + attrs)
- **Lines** (10 vertices): ~120-150 bytes
- **Polygons** (5 vertices): ~62 bytes
- **Complex streets**: ~300+ bytes

Estimated average across Denmark's 16M features: **~100 bytes per record**. Including the 32-byte SortRecord struct overhead, each record consumes ~132 bytes in-memory.

#### 1.5 Chunk Counts at Various Scales

| Dataset | Features | Sort data volume | Chunks at 1 GB | Chunks at 512 MB | Chunks at 256 MB |
|---------|----------|-----------------|----------------|------------------|------------------|
| Denmark (483 MB) | 16M | ~1.6 GB | 2 | 4 | 7 |
| Germany (4.4 GB) | 147M | ~14.7 GB | ~15 | ~30 | ~59 |
| Japan (2.3 GB) | 74M | ~7.4 GB | ~8 | ~15 | ~30 |
| Planet (est.) | 2.4B | ~240 GB | ~240 | ~480 | ~960 |

#### 1.6 The TilegenConfig Struct

At `/home/folk/Programs/elivagar/src/pipeline.rs:77-110`, `TilegenConfig` holds all pipeline configuration. It is the public API surface, re-exported from `/home/folk/Programs/elivagar/src/lib.rs:61`:
```rust
pub use pipeline::{run, PipelineError, SkipTo, TilegenConfig};
```

Adding a new field to `TilegenConfig` changes the public API. The lib.rs docstring (line 13-26) has an example that constructs `TilegenConfig` directly -- it would need updating.

### 2. Tradeoff Analysis

#### 2.1 Smaller Chunks: More Files, Deeper Merge Heap

The k-way merge uses a `BinaryHeap<HeapEntry>` where each heap operation is O(log k). Each `SortReader::next()` call does one heap pop + one heap push = 2 * O(log k) comparisons.

| Chunk budget | Planet chunks (k) | log2(k) | Heap ops/record | Read buffers (k * 256 KB) |
|-------------|-------------------|---------|-----------------|--------------------------|
| 1 GB | 240 | 7.9 | 16 | 60 MB |
| 512 MB | 480 | 8.9 | 18 | 120 MB |
| 256 MB | 960 | 9.9 | 20 | 240 MB |
| 128 MB | 1920 | 10.9 | 22 | 480 MB |

The heap operations are trivially cheap -- u64 comparisons. The real costs are:
1. **Read buffer memory**: 256 KB per chunk file's BufReader. At 960 chunks, that is 240 MB. Significant but manageable.
2. **File descriptors**: 960 open files. Linux default `ulimit -n` is typically 1024. Planet at 128 MB chunks would **hit the FD limit**. This is a hard lower bound.
3. **In-chunk sort cost**: Each chunk is sorted independently with `sort_unstable_by_key`. Smaller chunks mean more sort calls, but total records sorted is the same. Actually, smaller chunks give a slight advantage because in-memory sorts are more cache-friendly.
4. **Filesystem overhead**: More chunk files on disk, but sequential reads are unaffected by file count.

#### 2.2 Minimum Viable Chunk Size

**Hard minimum**: ~64 MB (below this, file descriptor limits become a problem for planet, and read buffer overhead becomes significant)

**Practical minimum**: 128 MB -- at planet scale this produces ~1920 chunks. With 256 KB read buffers, that is 480 MB just for BufReaders. The FD limit would need raising (`ulimit -n 4096` or similar).

**Safe minimum**: 256 MB -- at planet scale this is ~960 chunks, 240 MB read buffers, still under default FD limits.

#### 2.3 Sort Phase Time Impact (Estimated)

For Denmark (sort currently ~0.3-0.6s, negligible fraction of total):
- Halving chunk size: negligible impact. Sort phase is I/O-bound on the merge, and 4 chunks vs 2 chunks is irrelevant.

For planet (sort projected ~60-120s based on 240 GB I/O):
- 1 GB chunks (240-way merge): baseline
- 512 MB chunks (480-way merge): +5-10% sort time (deeper heap, more read buffers competing for cache)
- 256 MB chunks (960-way merge): +15-25% sort time (significant BufReader overhead, FD pressure)

Sort phase is consistently <1% of total wall time across all measured datasets. Even a 50% increase in sort time would be invisible.

#### 2.4 Real Memory Savings

The sort buffer is active during the PBF phase (phase12), where it coexists with:
- SortedNodeStore: ~51 GB at planet scale
- Way index (mmap): ~144 GB virtual, but dropped before sort phase
- In-flight way processing buffers: variable, controlled by MAX_INFLIGHT=4
- Relation batch buffers: REL_BATCH_SIZE=1024

Peak RSS pressure occurs during phase12 when the sort buffer, node store, and in-flight processing all overlap. Reducing the sort buffer from 1 GB to 256 MB saves ~750 MB of peak RSS during this critical phase. On a 64 GB machine with 51 GB in the node store, this is meaningful.

### 3. Concrete Action Plan

#### 3.1 Add `sort_chunk_size` to TilegenConfig

**File**: `/home/folk/Programs/elivagar/src/pipeline.rs`

Change the constant from a hardcoded value to a default, and add the field to the config:

1. Keep `SORT_CHUNK_SIZE` as the default value (renamed to `DEFAULT_SORT_CHUNK_SIZE` for clarity).
2. Add `pub sort_chunk_size: usize` to `TilegenConfig` (line ~110, after `threads`).
3. Update both `SortWriter::new` call (line 335) and `SortWriter::resume` call (line 188) to use `config.sort_chunk_size` instead of the constant.
4. Add a validation check in `run()` (near line 135): minimum 64 MB, warn below 256 MB.

**File**: `/home/folk/Programs/elivagar/src/lib.rs`

Update the docstring example (line 13-26) to include the new field with its default value.

#### 3.2 Add CLI Flag

**File**: `/home/folk/Programs/elivagar/src/main.rs`

Add `--sort-budget <bytes>` flag that accepts human-readable sizes. The flag parsing (line 33-88) follows a clear pattern -- new flags are added to the match block.

Recommended parsing: accept raw bytes, or suffixed values like `256M`, `1G`, `512M`.

Update the usage string (line 12) to include the new flag.

Set a `sort_chunk_size` local variable with default `1 << 30` (1 GB) that flows into the `TilegenConfig`.

#### 3.3 Suggested Profiles

Rather than named profiles, expose a simple numeric CLI flag. The documented guidance:

| Use case | `--sort-budget` | Rationale |
|----------|----------------|-----------|
| Default (high throughput) | 1G (default, no flag needed) | Current behavior. Best for machines with headroom. |
| Planet on 64 GB | 512M | Saves ~500 MB peak RSS. Negligible sort overhead. |
| Planet on 64 GB tight | 256M | Saves ~750 MB peak RSS. Minor sort overhead. Safe FD count. |
| Debug/tiny RAM | 128M | Maximum savings. Needs `ulimit -n 4096` at planet. |

Auto-detection is explicitly **not recommended** at this stage. Here is why:

1. Reading `/proc/meminfo` introduces platform-specific complexity (non-portable to macOS).
2. "Available memory" is a noisy signal -- the kernel reports MemAvailable which includes reclaimable page cache, but elivagar's mmaps and the node store both compete for that cache.
3. The sort buffer is just one of several memory consumers. Without controlling relation batches, assemble batches, and PMTiles directories simultaneously, auto-sizing one knob gives false confidence.
4. The user (or brokkr wrapper) is in a better position to decide. The `brokkr run --mem 8G` already wraps elivagar in a cgroup for OOM protection.

**Recommendation**: Document the profiles. If auto-detection is desired later, implement it in brokkr (which already knows the host and can query memory), not in elivagar's core library.

#### 3.4 Ocean Byte Accounting Fix

**File**: `/home/folk/Programs/elivagar/src/ocean.rs`

Line 318 uses `r.data.len() + 8` for byte tracking. This should be `r.data.len() + std::mem::size_of::<sort::SortRecord>()` (= 32) to match the sort writer's accounting. This is a minor correctness fix that should be included in the same change.

#### 3.5 File Descriptor Safety

**File**: `/home/folk/Programs/elivagar/src/pipeline.rs`

Add a warning (or error) when the computed number of chunks exceeds a threshold. After `SortWriter::finish()` is called (line 243), the chunk count is known. Before `SortReader::from_dir()`, check:

```
if chunk_count > 800 {
    eprintln!("Warning: {chunk_count} sort chunks — ensure ulimit -n is >= {}", chunk_count + 100);
}
```

This is purely informational. The OS will fail the file opens in `SortReader::new()` with a clear error if the limit is exceeded.

#### 3.6 Exact Code Locations to Modify

| File | Line(s) | Change |
|------|---------|--------|
| `src/pipeline.rs` | 117 | Rename constant to `DEFAULT_SORT_CHUNK_SIZE` |
| `src/pipeline.rs` | 77-110 | Add `sort_chunk_size: usize` field to `TilegenConfig` |
| `src/pipeline.rs` | 135-146 | Add validation: minimum 64 MB, warn below 256 MB |
| `src/pipeline.rs` | 188 | Change `SORT_CHUNK_SIZE` to `config.sort_chunk_size` |
| `src/pipeline.rs` | 335 | Change `SORT_CHUNK_SIZE` to `config.sort_chunk_size` |
| `src/main.rs` | 12 | Update usage string |
| `src/main.rs` | 25-29 | Add `sort_chunk_size` variable with default |
| `src/main.rs` | 33-88 | Add `--sort-budget` to match block |
| `src/main.rs` | 97-110 | Include `sort_chunk_size` in TilegenConfig construction |
| `src/lib.rs` | 13-26 | Update docstring example |
| `src/ocean.rs` | 318 | Fix byte accounting to use `+ 32` instead of `+ 8` |

### 4. Risk Assessment

**What breaks if chunks are too small?**

1. **File descriptor exhaustion** (hard failure): If `chunk_count` exceeds `ulimit -n`, `SortReader::new()` will fail with "Too many open files" on the (k+1)th `File::open`. The error propagates cleanly as `io::Error`. **Mitigation**: Validate and warn. Document minimum `ulimit -n` for low-budget configs.

2. **Read buffer memory overhead** (soft failure): At 960 chunks with 256 KB buffers, BufReaders alone consume 240 MB. This is RAM that was supposed to be saved. At 128 MB chunk budget, the BufReader overhead (480 MB) **exceeds the savings** from halving chunk size (375 MB saved). There is a crossover point where smaller chunks become counterproductive. **Mitigation**: Set minimum at 256 MB; document the tradeoff.

3. **Chunk file format: u32 record_count** (hard failure): The chunk file format uses `u32` for record count (sort.rs line 209: `let count = records.len() as u32`). At 1 GB chunks with ~100-byte average records, that is ~7.6M records, well within u32 range. Even at 64 MB chunks, records per chunk drops to ~485K. No overflow risk regardless of budget. **No mitigation needed.**

4. **Checkpoint compatibility** (behavioral): The checkpoint file stores `chunk_count` for `--skip-to` resume. If a user runs phase12 with one budget, then resumes with a different budget, the new budget applies to ocean/sort phases but not to existing PBF chunks. This is fine -- each chunk is self-contained. The only subtlety: the resume call uses the new budget for subsequent flushes, so chunks will have mixed sizes. The merge handles this correctly since chunks are independent. **No mitigation needed.**

5. **Sort stability**: `sort_unstable_by_key` produces the same global order regardless of chunk count. The k-way merge is deterministic for a given set of chunks. Output is identical. **No risk.**

### 5. Implementation Sequence

1. **Rename constant** to `DEFAULT_SORT_CHUNK_SIZE` (purely cosmetic, no behavior change)
2. **Add field** to `TilegenConfig` with default value
3. **Update pipeline** to use config field instead of constant (2 call sites)
4. **Add CLI flag** parsing in main.rs
5. **Fix ocean byte accounting** (line 318 in ocean.rs)
6. **Update lib.rs docstring** example
7. **Add validation** (minimum, FD warning)
8. **Run `brokkr check`** to verify tests pass
9. **Run `brokkr bench pmtiles`** -- sort budget does not affect PMTiles benchmark, this is a sanity check
10. **Run Denmark with `--sort-budget 256M`** to verify correct operation with smaller chunks and compare output

### Critical Files

- `/home/folk/Programs/elivagar/src/pipeline.rs` - Core change: constant, config struct, sort writer instantiation (3 changes)
- `/home/folk/Programs/elivagar/src/main.rs` - CLI flag parsing and TilegenConfig construction
- `/home/folk/Programs/elivagar/src/sort.rs` - No changes needed, but essential context: the SortWriter API already accepts chunk_size_bytes as a constructor parameter, so the plumbing is already in place
- `/home/folk/Programs/elivagar/src/ocean.rs` - Fix byte accounting inconsistency (line 318: `+ 8` should be `+ 32`)
- `/home/folk/Programs/elivagar/src/lib.rs` - Update public API docstring example to include new field
