# Box 1: Pipeline Orchestration and Phase Boundaries — Deep Investigation

Date: 2026-02-28
Scope: `src/pipeline.rs`, `src/main.rs`, with references to `src/sort.rs`, `src/node_index.rs`, `src/way_index.rs`, `src/ocean.rs`, `src/pmtiles_writer.rs`

## Overview

The pipeline orchestrator (`src/pipeline.rs`) sequences four phases through a single public entry point `run(&TilegenConfig)`. The phases are strictly sequential with hard boundaries between them. This document maps every resource lifecycle, threading decision, and handoff point to exact code locations.

---

## Phase Lifecycle — Complete Picture

### Phase 1+2: PBF Read + Feature Processing

**Entry**: `phase_read_and_process()` at `pipeline.rs:318`
**Denmark timing**: ~9.2s
**What happens**: Single-pass PBF read that simultaneously builds node/way indices AND processes features into sort records.

**Resources allocated at phase start:**
- `SortWriter` — in-memory record buffer + chunk directory (`pipeline.rs:322-324`). Buffer starts empty, grows to ~1 GB before flushing.
- `NodeStore` — either `SortedNodeStore::new()` (empty Vec of groups, `pipeline.rs:342`) or `NodeIndex::create()` (1 GB sparse mmap, `pipeline.rs:345`).
- `WayIndex::create()` — two sparse mmap files: `way_offsets.bin` (1 GB initial) + `way_data.bin` via BufWriter (`pipeline.rs:348`, `way_index.rs:35-69`).
- `LandMask` — 32 MB heap-allocated bitfield via `Arc` (`pipeline.rs:355`, `geometry.rs:1221`).
- `ElementReader` — PBF reader with decode thread pool (`pipeline.rs:328-331`).
- Reusable `node_records: Vec<SortRecord>` buffer (`pipeline.rs:380`).

**Sub-phases within Phase 1+2:**

1. **Node sub-phase** (inline on main thread):
   - Iterates DenseNode/Node blocks from PBF (`pipeline.rs:424-431`).
   - Each node: store in NodeStore, update bounds, check tags, emit POI records.
   - Node records pushed to SortWriter on main thread (`pipeline.rs:408-411`).
   - Nodes are processed inline because they are I/O-light (just store coords).

2. **Way sub-phase** (parallel, worker+drain architecture):
   - Triggered on first Way block (`pipeline.rs:432-523`).
   - **On first way block** (`pipeline.rs:440-518`):
     - `node_store_opt.take()` consumes the NodeStore and calls `into_reader()` (`pipeline.rs:441-444`).
       - For SortedNodeStore: `flush_chunk()` + `flush_group()` finalizes, moves `groups` Vec into `SortedNodeStoreReader` (`node_index.rs:749-797`). The groups Vec ownership transfers — **no copy, no extra memory**.
       - For flat NodeIndex: `mmap.make_read_only()` — converts MmapMut to Mmap (`node_index.rs:100-104`). **No memory change**.
     - NodeStoreReader wrapped in `Arc` for sharing across rayon threads (`pipeline.rs:443-445`).
     - Spawns **worker thread** (`pipeline.rs:458-503`): receives PrimitiveBlocks, extracts RawWay data, dispatches to rayon via `in_place_scope`.
     - Spawns **drain thread** (`pipeline.rs:509-515`): owns `way_index` + `sort_writer`, writes ProcessedWay results sequentially.
     - `block_tx` channel (sync_channel(1)) for blocks, `rtx/rrx` channel (sync_channel(4)) for results (`pipeline.rs:448-449`).
     - Token semaphore `MAX_INFLIGHT=4` limits concurrent rayon tasks (`pipeline.rs:457, 460-464`).
   - **Each way block**: main thread sends block to worker via `block_tx` (`pipeline.rs:521-522`).
   - **Worker thread**: extracts `RawWay` (owned tags via `to_string()`), spawns rayon task per block, processes via `process_raw_way()` which does node coord resolution + tag matching + geometry.
   - **Drain thread**: receives Vec<ProcessedWay>, writes way_index entries + sort records.

3. **Relation sub-phase** (batched parallel):
   - Triggered on first Relations block (`pipeline.rs:524-570`).
   - **Shuts down worker+drain threads** (`pipeline.rs:528-539`): drops `block_tx`, joins worker, joins drain, recovers `way_index` and `sort_writer`.
   - **Finalizes way_index** (`pipeline.rs:540-546`): `finish_writing()` flushes BufWriter, opens read-only mmap over `way_data.bin` (`way_index.rs:114-128`).
   - Relations processed in batches of `REL_BATCH_SIZE=1024` (`pipeline.rs:822`):
     - `prepare_relation()`: resolves member ways from way_index (serial I/O reads from mmap), does tag matching while PBF borrows are alive (`pipeline.rs:829-878`).
     - `flush_rel_batch()`: parallel via `into_par_iter()` — each relation runs `process_prepared_relation()` on rayon, then serial push to sort_writer (`pipeline.rs:882-905`).

**Resources released at phase end:**
- `way_index` explicitly dropped at `pipeline.rs:596` — releases `way_offsets.bin` + `way_data.bin` mmaps. Comment says "For North America this frees ~11 GB from RSS."
- `NodeStoreReader` (Arc) — still alive in the Arc, but the worker and drain threads are joined, so only the main thread's Arc reference remains. The `Arc` is NOT explicitly dropped before phase end. It was created at `pipeline.rs:443` and lives until `phase_read_and_process` returns.
  - **VERIFIED**: The `nr` Arc is created in the scope at `pipeline.rs:443`, cloned once to `nr_clone` at `pipeline.rs:450`. `nr_clone` is moved into the worker thread which is joined before the function returns. So after worker join, only `nr` remains. But `nr` is a local variable — it drops when `phase_read_and_process()` returns at `pipeline.rs:623`.
  - **INFERENCE**: The NodeStoreReader (44-52 GB at planet scale) persists through the entire relation sub-phase. This is necessary because relation processing needs node coords? **NO** — relations use `way_index.get()` not `node_reader.get()`. The NodeStoreReader is NOT needed after the way sub-phase ends.

**FINDING: NodeStoreReader is held ~unnecessarily during the entire relation sub-phase.** At planet scale with 44-52 GB for the SortedNodeStore, this means those 44-52 GB are pinned in RAM while relation processing runs. Relations only need the way_index, not the node store. Dropping the NodeStoreReader Arc before relation processing would reclaim 44-52 GB.

**Peak memory during phase:**
- Denmark: SortedNodeStore ~420 MB + WayIndex ~a few hundred MB + SortWriter buffer up to 1 GB + LandMask 32 MB + misc = ~869 MB RSS.
- Planet: SortedNodeStore 44-52 GB + WayIndex ~10 GB + SortWriter buffer 1 GB + LandMask 32 MB = ~55-63 GB. Way index dropped at end frees ~10 GB.

**I/O:**
- PBF sequential read (pipelined I/O thread in pbfhogg).
- SortWriter flushes chunk files to `data/tilegen_tmp/sort_chunks/` when buffer exceeds 1 GB.
- WayIndex appends to `way_data.bin` (sequential), random writes to `way_offsets.bin` (sparse mmap).
- NodeIndex (flat path only): random writes to `nodes.idx` (sparse mmap, 96 GB at planet).

**Parallelism:**
- pbfhogg decode: `threads/3` threads (decode pool).
- Rayon global pool: `threads` threads total (configured in `main.rs:92-95`).
- Worker thread + drain thread + main thread + rayon pool.
- During way sub-phase: main thread forwards blocks, worker thread extracts + dispatches to rayon, drain thread writes results. Up to `MAX_INFLIGHT=4` rayon tasks in flight.
- During relation sub-phase: main thread runs `prepare_relation()` serially, `flush_rel_batch()` dispatches to rayon.

---

### Ocean Phase

**Entry**: Ocean processing block in `run()` at `pipeline.rs:195-226`
**Denmark timing**: ~1.4s
**What happens**: Reads ocean shapefile, parses polygons, pre-splits large ones, processes in parallel with rayon.

**Resources at start:**
- `SortWriter` — carries over from phase 1+2 (or resumed from checkpoint).
- `data_bounds` — loaded from checkpoint file (`pipeline.rs:189`).
- `land_mask` — loaded from file or carried over (`pipeline.rs:190`).
- Ocean shapefile mmap'd (`ocean.rs:79`).

**Resources allocated:**
- `polygons: Vec<OceanPolygon>` — all parsed polygons collected in memory (`ocean.rs:89`). At planet scale, this could be 1-5 GB (per Box 5 investigation).
- Pre-split output `split_out: Vec<OceanPolygon>` — may be larger than input (`ocean.rs:204`).
- Per-rayon-worker `OceanAcc` — thread-local record buffers + chunk file paths (`ocean.rs:280-286`).

**Parallel processing:**
- `par_iter().fold().map().reduce()` pattern (`ocean.rs:305-337`).
- Each rayon worker flushes its own chunk files directly when buffer exceeds chunk_size (`ocean.rs:289-303`). Chunk IDs allocated via `AtomicUsize` (`ocean.rs:276`).
- Chunk files use same `chunk_NNNN.bin` naming, starting after PBF chunk count.

**Resources released at end:**
- `polygons` Vec dropped after `par_iter` consumes it.
- Shapefile mmap dropped when `process_ocean_shapefile()` returns.
- `sort_writer.adopt_chunk_files()` adds ocean chunk paths to the writer (`ocean.rs:339`).

**I/O:**
- Shapefile .shx read (full read to memory), .shp mmap.
- Sort chunk files written in parallel by rayon workers.

**What's NOT released:**
- SortWriter in-memory buffer (may have unflushed records from ocean tail).
- LandMask (32 MB, carried through).

---

### Phase 3: Sort

**Entry**: `pipeline.rs:232-239`
**Denmark timing**: ~0.5s
**What happens**: Converts SortWriter to SortReader (flushes final buffer + creates merge reader).

**Detailed flow:**
1. `sort_writer.finish()` called at `pipeline.rs:235`:
   - Flushes remaining in-memory buffer as a final chunk (`sort.rs:147-149`).
   - Creates `SortReader::new()` from all chunk paths (`sort.rs:151`).
2. `SortReader::new()` opens all chunk files and primes the merge heap (`sort.rs:335-355`):
   - Opens each chunk file as `BufReader<File>` (256 KB buffer, `sort.rs:236`).
   - Reads first record from each chunk, pushes onto `BinaryHeap`.
   - Heap size = number of chunks (Denmark: ~2-4, Planet: ~20+).

**OR** if `--skip-to sort`:
- `SortReader::from_dir()` scans directory for `chunk_NNNN.bin` files (`sort.rs:318-331`, `pipeline.rs:237`).

**Resources allocated:**
- `SortReader` — k open file handles + BinaryHeap of HeapEntry (40 bytes each, `sort.rs:279`).
- Each ChunkReader has a 256 KB BufReader (`sort.rs:236`).

**Resources released:**
- SortWriter's in-memory buffer (after flush_chunk + clear, `sort.rs:180-181`).
- SortWriter struct itself (consumed by `finish()`).

**Peak memory:** Minimal — just the k BufReaders + heap. Each HeapEntry is 40 bytes + one record's data Vec. At planet scale with ~20 chunks: ~20 * 256 KB = 5 MB for BufReaders + small heap.

**I/O:** Chunk files opened for reading. Sequential reads during merge (triggered in phase 4).

---

### Phase 4: Tile Assembly

**Entry**: `phase_assemble()` at `pipeline.rs:1320`
**Denmark timing**: ~2.5s
**What happens**: 3-thread pipeline reads merged sort records, encodes MVT tiles in parallel, writes PMTiles archive.

**Resources allocated:**
- `PmtilesWriter` — streaming mode: two temp files (`tiles.blob`, `dir_entries.bin`) in tmp_dir (`pmtiles_writer.rs:160-178`, `pipeline.rs:1332`).
- Two `sync_channel(1)` channels (`pipeline.rs:1342-1343`):
  - `(read_tx, read_rx)` — reader thread to encoder (main thread).
  - `(encode_tx, encode_rx)` — encoder (main thread) to writer thread.
- Batch buffers: `Vec<PendingTile>` with capacity `BATCH_SIZE=4096` (`pipeline.rs:1349`).

**3-thread pipeline architecture:**

1. **Reader thread** (`pipeline.rs:1347-1389`):
   - Calls `sort_reader.next()` in a loop — drives the k-way merge.
   - Groups records by tile_id into `PendingTile` structs.
   - Skips ocean-only tiles via `should_emit()` (`pipeline.rs:1356-1358`).
   - Sends batches of 4096 tiles via `read_tx`.

2. **Main thread (encoder)** (`pipeline.rs:1421-1425`):
   - Receives batches from `read_rx`.
   - Calls `encode_tile_batch()` which uses `batch.par_iter().map_init()` — rayon parallel encoding (`pipeline.rs:1453-1524`).
   - Each rayon worker: decode features into LayerBuilders, merge same-attr geometries, encode MVT protobuf, gzip compress with libdeflater.
   - `map_init()` creates per-thread scratch buffers + compressor — reused across tiles (`pipeline.rs:1458-1465`).
   - Forwards encoded batch to `encode_tx`.

3. **Writer thread** (`pipeline.rs:1393-1417`):
   - Receives encoded batches from `encode_rx`.
   - Calls `pmtiles.add_tile()` for each — writes compressed data to blob file, records directory entry.
   - Tracks per-zoom tile counts and bytes.

**After scope exits (`pipeline.rs:1432-1435`):**
- `pmtiles.write_to(&config.output_path)` — finalization:
  - Calls `collect_dir_entries()` — reads back `dir_entries.bin` into Vec<DirEntry> (`pmtiles_writer.rs:340-363`).
  - Builds root + leaf directories (`pmtiles_writer.rs:366`).
  - Compresses metadata JSON.
  - Reads back `tiles.blob` and writes final PMTiles archive.
  - Per Box 8: ~240 MB transient spike from entries Vec + dedup HashMap.

**Resources released:**
- SortReader (k file handles) — consumed during reader thread.
- PmtilesWriter temp files cleaned up after write_to.
- All sort chunk files on disk remain (not cleaned up by pipeline — `brokkr clean` handles this).

**Parallelism:**
- Reader thread: 1 dedicated thread (k-way merge is sequential).
- Encoder: main thread + rayon pool (all threads).
- Writer thread: 1 dedicated thread (PMTiles writes are sequential).
- Total: 2 dedicated threads + main thread + rayon pool.

**Peak memory:**
- In-flight batches: up to 2 batches (one being read, one being encoded, one being written — but channel depth 1 means at most 2 in flight).
- Per-tile: `PendingTile` (32 bytes + features Vec), `EncodedTile` (32 bytes + compressed Vec).
- Rayon per-thread: scratch buffers + compressor (~few KB each).
- PMTiles writer: dedup HashMap (up to 1M entries = ~50 MB), blob file handle.

---

## Memory Timeline

### Denmark (~870 MB peak RSS)

```
Time ──────────────────────────────────────────────────────────────────>
Phase:  |------------ Phase 1+2 (9.2s) ------------|--Ocean (1.4s)--|Sort|-- Phase 4 (2.5s) --|

Node    ████████████████████████████████████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
Store    ~420 MB (SortedNodeStore)      into_reader() at first Way     drops when fn returns
         Grows during node sub-phase    No memory change (move)        ~line 623

Way     ░░░░░░░░░████████████████████████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
Index    Created   Grows during way phase  finish_writing()  drop at line 596
         ~1 GB     offsets: sparse mmap     opens read mmap

Sort    ████████████████████████████████████████████████████████████████░░░░░░░░░░░░░░░░░░░░░░
Writer   Buffer grows to ~1 GB, flushes, grows again        finish() → SortReader
         Multiple flush cycles during ways+rels+ocean

Land    ████████████████████████████████████████████████████████████████░░░░░░░░░░░░░░░░░░░░░░
Mask     32 MB, Arc'd during ways, unwrapped, saved to disk  loaded for ocean

Sort    ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░██████████████████████
Reader   Created at sort phase start                                   ~5 MB (k BufReaders)

PMTiles ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░████████████████████
Writer   Streaming temp files + dedup map                               finalize spike ~240MB
```

### Planet Scale (~64 GB machine) — PROJECTED

```
Time ──────────────────────────────────────────────────────────────────>
Phase:  |------------ Phase 1+2 (???) --------------|--Ocean (??)--|Sort|-- Phase 4 (???) --|

Node    ████████████████████████████████████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
Store    44-52 GB SortedNodeStore        into_reader() at first Way    drops when fn returns
         WARNING: leaves only 12-20 GB headroom!

Way     ░░░░░░░░░████████████████████████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
Index    Created   Grows during way phase  drop at line 596
         offsets: up to ~10 GB (way IDs to ~1B)
         data: ~?? GB (depends on total way coords)

Sort    ████████████████████████████████████████████████████████████████░░░░░░░░░░░░░░░░░░░░░░
Writer   1 GB buffer, flushes ~20+ chunk files

PEAK    |<---- 44-52 GB node + ~10 GB way + 1 GB sort + 0.03 GB mask = 55-63 GB ---->|
RSS      ^^^^^^^^^ THIS IS THE CRITICAL BOTTLENECK ^^^^^^^^^

Ocean   ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░
Polys    Parsed into Vec: 1-5 GB                   dropped after par_iter
```

**Critical observation**: At planet scale, NodeStore (44-52 GB) + WayIndex (~10 GB) + SortWriter buffer (1 GB) = 55-63 GB. On a 64 GB machine, this leaves only 1-9 GB for everything else (page cache, stack, OS, etc.). The pipeline will likely OOM or thrash.

**Additional critical observation**: The NodeStoreReader lives until `phase_read_and_process()` returns. It is NOT needed during the relation sub-phase. Dropping it before relations could reclaim 44-52 GB. However, this is less actionable than it sounds — the relation sub-phase overlaps with nothing, and the memory is needed for the way sub-phase. The real issue is that NodeStore + WayIndex coexist during the way sub-phase.

---

## Thread Pool and Parallelism

### Rayon Global Pool Configuration

**Location**: `main.rs:92-95`
```rust
rayon::ThreadPoolBuilder::new()
    .num_threads(threads)
    .build_global()
    .expect("failed to configure rayon thread pool");
```

**Default**: `std::thread::available_parallelism()` — logical CPU count (`main.rs:27-29`).
**Override**: `-j N` or `--threads N` CLI flag (`main.rs:65-73`).

The rayon pool is built globally before any pipeline work. All `par_iter()` calls use this pool.

### Decode Thread Split: `threads/3`

**Location**: `pipeline.rs:327`
```rust
let decode_threads = (config.threads / 3).max(1);
```

**How it works**: pbfhogg's `decode_threads()` sets the number of threads in its internal decode pool. On a 24-thread machine: 24/3 = 8 decode threads. On a 12-thread machine: 12/3 = 4 decode threads.

**Interaction with rayon**: The decode threads are separate from rayon's pool. pbfhogg's `into_blocks_pipelined()` uses its own thread pool (I/O thread + N decode threads). Rayon's pool handles way processing, relation processing, and tile encoding.

**The theoretical review's concern**: "Static decode thread split may be suboptimal across hardware/storage profiles." This is a valid concern. The split is a fixed fraction regardless of:
- Storage speed (NVMe vs SATA vs network)
- CPU vs I/O boundedness of the workload
- Whether the PBF is large (planet) or small (Denmark)

**Verified**: The `threads` value controls rayon's pool size AND the decode fraction. There is no separate control for decode threads.

### Way Sub-Phase Threading Architecture

During the way sub-phase, the threading is:

```
pbfhogg I/O thread
    → pbfhogg decode pool (threads/3 threads)
        → PrimitiveBlock delivered to main thread (into_blocks_pipelined iterator)
            → Main thread sends block to worker thread (sync_channel(1))
                → Worker thread: extracts RawWay, spawns rayon task
                    → Rayon pool: process_raw_way() in parallel
                        → Results sent to drain thread (sync_channel(4))
                            → Drain thread: writes way_index + sort records
```

**Backpressure points:**
1. `block_tx.send(block)` blocks if worker is busy (`pipeline.rs:521-522`, channel depth 1).
2. `token_rx.recv()` blocks if 4 rayon tasks are already in-flight (`pipeline.rs:485`).
3. `rtx.send(results)` can block if drain thread is slow (channel depth 4, `pipeline.rs:449`).

**Contention analysis:**
- The drain thread is the single serial bottleneck: `way_index.put()` (mmap write) + `sort_writer.push()` (buffer + occasional flush). If drain falls behind, the result channel fills up (depth 4), which blocks rayon task completion, which blocks token return, which blocks the worker from accepting new blocks.
- **Verified**: The drain thread does NOT use rayon. It runs on its own dedicated thread. So it doesn't compete for rayon pool threads.

### Assembly Phase Threading

During assembly, the threading is:

```
Reader thread: sort_reader.next() loop → batch of PendingTile
    → sync_channel(1) → Main thread: encode_tile_batch() → rayon par_iter
                          → sync_channel(1) → Writer thread: pmtiles.add_tile()
```

**Contention:**
- Reader thread is I/O-bound (sequential chunk file reads).
- Main thread + rayon pool are CPU-bound (MVT encode + gzip).
- Writer thread is I/O-bound (sequential blob file writes).
- With `sync_channel(1)`, at most 1 batch can be buffered between each stage. If writer stalls (disk I/O), encoder stalls (can't send), reader stalls (can't send).

**Thread count**: 2 dedicated threads + main thread + rayon pool. On a 24-thread machine, rayon has 24 threads, but the main thread is blocked in `for batch in read_rx` — it only wakes when a batch arrives. So effectively: 1 reader + 1 writer + rayon pool (main thread drives rayon's `par_iter`).

---

## Sort Chunk Management

### SortWriter Mechanics

**Location**: `sort.rs:67-184`

**Buffer structure**: `Vec<SortRecord>` (`sort.rs:69`), sized in bytes by `buffer_bytes` (`sort.rs:70`).

**Flush policy**: When `buffer_bytes >= chunk_size_bytes` (default 1 GB), call `flush_chunk()` (`sort.rs:139-141`).

**How buffer_bytes is tracked**: Each `push()` adds `record.data.len() + 8` (8 for the key) (`sort.rs:137`). This is approximate — doesn't account for Vec overhead (32 bytes per SortRecord for key + Vec ptr/len/cap).

**FINDING**: The `buffer_bytes` underestimates actual memory usage. Each `SortRecord` is 32 bytes (key u64 + Vec<u8> which is ptr+len+cap=24), but only `data.len() + 8` is tracked. The Vec overhead (24 bytes for the Vec itself, minus the 8 counted for the key) means ~16 bytes per record unaccounted. With average data size of ~50-100 bytes, this is ~15-25% undercount. For Denmark this doesn't matter, but at planet scale with hundreds of millions of records in a 1 GB buffer, actual memory could be ~1.2 GB when the threshold triggers.

**Flush process** (`sort.rs:174-183`):
1. `write_sorted_chunk(&mut self.buffer, &path)` — sorts in-place by key, writes to file.
2. File format: `u32 count | (u64 key, u32 data_len, [u8] data)*` (`sort.rs:188-222`).
3. BufWriter with 1 MB buffer (`sort.rs:205`).
4. After write: `buffer.clear()`, `buffer_bytes = 0`.

**Chunk naming**: `chunk_NNNN.bin` (zero-padded 4 digits, `sort.rs:175`). Sequential numbering.
**Chunk directory**: `data/tilegen_tmp/sort_chunks/` (`pipeline.rs:109`).

**Denmark chunk count**: With ~16M features at average ~50-100 bytes, total sort data is ~1-2 GB. That's 1-2 chunks for the PBF phase, plus ocean chunks. Likely 2-4 total.

**Planet projection**: Planet has ~8B nodes, ~1B ways, ~20M relations. With ~200M+ matched features at 50-100 bytes average, that's ~10-20 GB of sort data → ~10-20 chunks. Ocean adds more.

### Ocean Chunk Management

**Location**: `ocean.rs:256-339`

Ocean processing writes chunk files in parallel via rayon workers:
- Each worker has its own `OceanAcc` with a records buffer (`ocean.rs:280-286`).
- When buffer exceeds `chunk_size`, flush directly to a chunk file (`ocean.rs:289-303`).
- Chunk IDs allocated atomically starting after the PBF chunk count (`ocean.rs:276`).
- After all workers finish, `sort_writer.adopt_chunk_files()` adds paths (`ocean.rs:339`).

**Risk**: Multiple rayon workers could be flushing simultaneously, causing disk I/O contention. But chunk files are independent, so this is only sequential bandwidth.

---

## Phase Boundaries — Resource Handoffs

### Between Phase 1+2 and Ocean

**What's handed off:**
- `SortWriter` — returned from `phase_read_and_process()` as first tuple element (`pipeline.rs:623`).
- `data_bounds: MercBbox` — returned and saved to checkpoint (`pipeline.rs:172`).
- `land_mask: LandMask` — returned, saved to file, then passed as reference to ocean (`pipeline.rs:173, 190`).

**What's released:**
- `NodeStoreReader` (Arc) — dropped when `phase_read_and_process()` returns (`pipeline.rs:443` goes out of scope).
- `WayIndex` — explicitly dropped at `pipeline.rs:596` inside `phase_read_and_process()`.
- `ElementReader` — dropped when `for block_result in reader.into_blocks_pipelined()` loop ends.

**Memory freed**: NodeStore (~420 MB Denmark / 44-52 GB planet) + WayIndex (~few hundred MB Denmark / ~10 GB planet).

### Between Ocean and Sort

**What's handed off:**
- `sort_writer: Option<SortWriter>` — wrapped in `Some()` at `pipeline.rs:228`.

**What's released:**
- Ocean polygon Vec (dropped inside `process_ocean_shapefile`).
- Shapefile mmap (dropped inside `process_ocean_shapefile`).
- `land_mask` — not explicitly dropped, but it's a local in `run()`. It persists until `run()` returns.

**FINDING**: The `land_mask` (32 MB) persists through sort and assembly phases. It's only needed during PBF and ocean processing. At planet scale this is negligible, but it's technically unnecessary.

### Between Sort and Assembly

**What's handed off:**
- `sort_reader: SortReader` — either from `sort_writer.finish()` or `SortReader::from_dir()` (`pipeline.rs:234-238`).

**What's released:**
- `SortWriter` — consumed by `finish()`.
- In-memory sort buffer — flushed and cleared during `finish()`.

### NodeStore::into_reader() — Detailed Analysis

**For SortedNodeStore** (`node_index.rs:749-797`):
1. `self.flush_chunk()` — writes any pending chunk to the current group's data blob.
2. `self.flush_group()` — moves current group data into `self.groups[gid]`.
3. Diagnostic: scans all groups to compute blob bytes, chunk counts.
4. Returns `SortedNodeStoreReader { groups: self.groups }` — **moves the groups Vec**.
5. **Memory implication**: Zero additional memory. The groups Vec with all its Box<Group> blobs is moved wholesale into the reader. No copy.

**For NodeIndex (flat mmap)** (`node_index.rs:100-104`):
1. `self.mmap.make_read_only()` — converts MmapMut to read-only Mmap.
2. Returns `NodeIndexReader { mmap, file_len }`.
3. **Memory implication**: Zero additional memory. The mmap just changes protection flags.

### Could Phase Boundaries Be Relaxed?

**Ocean + PBF overlap**: Currently impossible. Ocean needs `data_bounds` which is only available after PBF phase completes (it's computed from all node coordinates). Also, ocean uses the LandMask which is populated during PBF processing.

**Sort + PBF/Ocean overlap**: Theoretically possible — the SortWriter's `flush_chunk()` already writes sorted chunks during PBF processing. But the merge (SortReader) requires all chunks to be finalized first. However, a streaming merge that accepts new chunks would be complex and the sort phase is only ~0.5s for Denmark.

**Sort + Assembly overlap**: Not possible with current design. Assembly reads from the SortReader which requires all chunks to be present.

**Ocean + Relation overlap**: Interesting possibility. Relations don't use ocean data, and ocean doesn't use relation data. If the ocean shapefile was processed on separate threads while relations were being processed on rayon, they could overlap. Both would need their own SortWriter (or thread-safe writes), which complicates things.

---

## Skip-to Mechanism

### --skip-to ocean

**Location**: `pipeline.rs:176-185`

**What happens:**
1. `load_checkpoint(&config.tmp_dir)` reads `checkpoint.txt` — gets `data_bounds` + `pbf_chunks` count (`pipeline.rs:177`).
2. `SortWriter::resume()` with `pbf_chunks` as `start_chunk` (`pipeline.rs:180`):
   - Verifies chunk files 0..pbf_chunks exist (`sort.rs:96-104`).
   - Deletes any chunks beyond `pbf_chunks` (leftover ocean/later chunks from previous run, `sort.rs:108-117`).
3. `load_land_mask()` reads `land_mask.bin` from tmp_dir (`pipeline.rs:181`).
4. PBF phase is skipped entirely. Ocean + sort + assembly run normally.

**Invariants:**
- Checkpoint file must exist with valid bounds + chunk count.
- All chunk files 0..pbf_chunks must exist and be valid.
- The PBF file must not have changed since the checkpoint was saved (no verification!).

**FINDING**: No integrity verification. If the PBF file changed between runs, the reused chunks contain stale data. No hash or timestamp check.

### --skip-to sort

**Location**: `pipeline.rs:157-162`

**What happens:**
1. Skips PBF phase, ocean phase, and goes straight to sort.
2. `SortReader::from_dir()` scans for all `chunk_NNNN.bin` files sequentially (`sort.rs:318-331`, `pipeline.rs:237`).

**Invariants:**
- At least one chunk file must exist in sort_chunks directory.
- All chunk files must be valid sorted format.

**Risks:**
- If a previous run crashed mid-ocean (writing chunk files), some ocean chunks may be partially written or missing. `SortReader::from_dir()` stops at the first gap (sequential scan), so partially-written chunks at the end would be included only if they parse correctly.
- **FINDING**: `SortReader::from_dir()` scans sequentially starting from 0, stops at first missing file. If chunk_0003.bin is missing but chunk_0004.bin exists, chunk_0004 is silently excluded. This is correct behavior for the skip-to-sort case.

---

## Error Handling and Recovery

### Phase Failures

**PBF phase**: Many operations use `expect()` (panic) inside the PBF callback because errors can't be propagated from pbfhogg callbacks. Panics at `pipeline.rs:410` (sort push), `pipeline.rs:522` (block send). The comment says "Disk I/O failure is unrecoverable mid-pipeline."

**Sort writer flush**: `flush_chunk()` returns `io::Result`, but callers inside PBF callbacks use `expect()` (`pipeline.rs:723`).

**Assembly**: Writer thread uses `expect()` for `add_tile()` failure (`pipeline.rs:1405`). The comment says "Panic: disk I/O failure is unrecoverable mid-pipeline."

### Temp File Cleanup

**On normal completion**: Temp files are NOT cleaned up. The `data/tilegen_tmp/` directory with all chunk files, node/way index files, PMTiles temp files remain.

**On crash**: Same — temp files remain. A subsequent run with `--skip-to` may find stale data. Without `--skip-to`, the `drop(std::fs::remove_dir_all(&config.tmp_dir))` at `pipeline.rs:166` cleans the directory on a fresh run.

**FINDING**: `remove_dir_all` is best-effort (result dropped). If it fails (e.g., permissions), the `create_dir_all` on the next line will succeed if the directory already exists, but stale files may persist inside it, specifically chunk files from a previous run that are outside the new run's numbering.

Wait — actually, the chunk directory is `config.tmp_dir.join(SORT_CHUNKS_DIR)` = `data/tilegen_tmp/sort_chunks/`. The `remove_dir_all` targets `config.tmp_dir` = `data/tilegen_tmp/`. So it removes everything recursively. If that fails, `create_dir_all` creates the directory fresh. The `SortWriter::new()` at `pipeline.rs:323` calls `fs::create_dir_all(tmp_dir)` for the sort_chunks dir, which succeeds whether it exists or not.

**Stale data risk**: If `remove_dir_all` fails and stale chunk files exist in `sort_chunks/`, the new `SortWriter` starts numbering from 0, potentially overwriting old files. Since numbering starts from 0 and goes up, any old files with higher numbers won't be overwritten but also won't be tracked by the new SortWriter's chunk_paths. They would only be a problem for `--skip-to sort` (which uses `from_dir` sequential scan).

### Could a Crash Leave Corrupted State?

**Yes**: If the process is killed during `flush_chunk()`, a chunk file may be partially written. The next `--skip-to sort` run would try to read it. `ChunkReader::open()` reads the record count from the first 4 bytes — if those bytes are garbage, it could read a wrong count and potentially crash or produce garbage output.

**Mitigation**: A fresh run (without `--skip-to`) removes the entire tmp directory, which cleans corrupted files.

---

## Config and Tunables

### Configurable via CLI (main.rs)

| Parameter | Flag | Default | Code |
|-----------|------|---------|------|
| PBF path | positional arg 1 | required | `main.rs:17` |
| Output path | positional arg 2 | required | `main.rs:18` |
| Tmp directory | `--tmp-dir` | `data/tilegen_tmp` | `main.rs:20,43` |
| Ocean shapefile | `--ocean` | None | `main.rs:44` |
| Ocean simplified | `--ocean-simplified` | None | `main.rs:45` |
| Skip-to phase | `--skip-to ocean\|sort` | None | `main.rs:46-54` |
| In-memory mode | `--in-memory` | false | `main.rs:77-79` |
| Compression level | `--compression-level 0-10` | 6 | `main.rs:56-63` |
| Force sorted PBF | `--force-sorted` | false | `main.rs:80-82` |
| Thread count | `-j N` or `--threads N` | available_parallelism | `main.rs:65-73` |
| Min zoom | hardcoded | 0 | `main.rs:102` |
| Max zoom | hardcoded | 14 | `main.rs:103` |

### Hardcoded That Should Be Tunable

| Parameter | Value | Location | Rationale for Tuning |
|-----------|-------|----------|---------------------|
| `SORT_CHUNK_SIZE` | 1 GB | `pipeline.rs:112` | Planet memory pressure. On 64 GB with 52 GB node store, 1 GB buffer is fine but tight. |
| `BATCH_SIZE` | 4096 | `pipeline.rs:1335` | Assemble pipeline batch size. May affect cache locality and encoding latency. |
| `sync_channel(1)` | depth 1 | `pipeline.rs:1342-1343` | Assemble pipeline buffering. Depth 2-4 could reduce stalls. |
| `MAX_INFLIGHT` | 4 | `pipeline.rs:457` | Way processing parallelism limit. |
| `decode_threads` | threads/3 | `pipeline.rs:327` | PBF decode pool size. |
| `REL_BATCH_SIZE` | 1024 | `pipeline.rs:822` | Relation batch size for parallel processing. |
| `MAX_DEDUP_ENTRIES` | 1M | `pmtiles_writer.rs:56` | PMTiles dedup map cap. |
| `SPLIT_Z` | 8 | `ocean.rs:200` | Ocean polygon pre-split zoom level. |
| `SPLIT_MIN_VERTICES` | 500 | `ocean.rs:201` | Threshold for ocean polygon pre-splitting. |

---

## Verification of Theoretical Review Claims

### Claim 1 (HIGH): Potential underlap in assemble pipeline due to fixed `sync_channel(1)`

**VERIFIED and EXPANDED.**

The assemble pipeline has two `sync_channel(1)` at `pipeline.rs:1342-1343`.

With depth 1, the pipeline can have at most:
- 1 batch being read by reader
- 1 batch buffered in read channel
- 1 batch being encoded by main+rayon
- 1 batch buffered in encode channel
- 1 batch being written by writer

That's 3 active + 2 buffered = 5 batches maximum in the system. But the critical path is:
- If the writer stalls on disk I/O, it can't recv from encode_rx.
- encode_tx.send() blocks because channel is full (depth 1, one batch already buffered).
- Main thread (encoder) stalls — can't send, so can't recv from read_rx.
- read_tx.send() blocks because channel is full.
- Reader thread stalls — can't read more records.

With depth 1, a single writer stall propagates instantly to all other threads. With depth 2+, the encoder could continue encoding one more batch while the writer catches up.

**Impact**: For Denmark (2.5s assemble, ~54K tiles = ~13 batches), stalls might be brief. For planet (~200M+ tiles = ~50K batches), sustained writer stalls could cause significant underlap.

**Note**: The `par_iter()` encoding step is CPU-bound. If encoding is slower than reading, the reader fills its channel quickly and stalls. If encoding is slower than writing, the writer drains quickly. The bottleneck shifts based on tile complexity.

### Claim 2 (MEDIUM): Static decode thread split (`threads/3`) may be suboptimal

**VERIFIED.**

`pipeline.rs:327`: `let decode_threads = (config.threads / 3).max(1);`

On a 24-thread machine: 8 decode threads, 24 rayon threads. On a 12-thread machine: 4 decode, 12 rayon.

**Problem**: The decode threads are separate from rayon. Total thread count during way processing:
- pbfhogg I/O thread: 1
- pbfhogg decode threads: threads/3
- Worker thread: 1
- Drain thread: 1
- Rayon pool: threads
- Total: threads + threads/3 + 3

On a 24-thread machine: 24 + 8 + 3 = 35 threads competing for 24 logical CPUs.

**Oversubscription**: The total thread count exceeds available CPU threads. However, many threads are I/O-bound (reader, writer, drain), so the actual CPU contention may be manageable. But on a 6-core machine (12 threads): 12 + 4 + 3 = 19 threads for 12 CPUs.

**The deeper issue**: During assembly phase, only the rayon pool + 2 dedicated threads are active (no pbfhogg). During PBF phase, both pbfhogg and rayon are active. The decode split should ideally be tuned per-phase, but it's set once at PBF reader creation.

### Claim 3 (MEDIUM): Fixed sort chunk target (1 GB) not adaptive to free RAM

**VERIFIED.**

`pipeline.rs:112`: `const SORT_CHUNK_SIZE: usize = 1 << 30;` (1 GB).

The buffer_bytes tracking underestimates actual memory (see Sort Chunk Management section above).

**Planet concern**: With NodeStore at 52 GB + WayIndex at ~10 GB on a 64 GB machine, the 1 GB sort buffer is one of the last remaining allocations that fit. If it grew larger (e.g., due to the underestimation allowing it to reach ~1.2 GB actual), it could push total RSS over the 64 GB limit.

**Adaptive alternative**: Could query available memory at runtime and size the chunk buffer accordingly. E.g., `min(1 GB, available_mem / 4)`.

---

## What the Review Might Have Missed

### 1. NodeStoreReader Lifetime Is Excessive

**CRITICAL finding confirmed by code analysis.**

The `NodeStoreReader` (Arc'd at `pipeline.rs:443`) lives until `phase_read_and_process()` returns at `pipeline.rs:623`. It is only needed during the way sub-phase — for `process_raw_way()` to resolve node coordinates. During the relation sub-phase, relations use `way_index.get()` exclusively.

At planet scale, the NodeStoreReader holds 44-52 GB. Dropping it before the relation sub-phase would reclaim all of that memory.

**How to fix**: After shutting down the worker+drain threads (which are the only other holders of the Arc), explicitly drop the `nr` Arc. Currently, `nr` is created at `pipeline.rs:443` inside a `if block_tx.is_none()` block. It's a local variable that lives until the enclosing scope ends.

Wait — actually, let me re-check. The `nr` variable is created inside the `if block_tx.is_none()` block at `pipeline.rs:441-445`. That block is inside the `BlockType::Ways` match arm at `pipeline.rs:432`. The `nr` variable is local to that inner block, so it drops when that block ends... but `nr_clone` is moved into the worker thread closure. After the worker thread is joined, `nr_clone` is dropped (it was moved into the `move` closure).

Actually, `nr` is created at `pipeline.rs:443` as `let nr = std::sync::Arc::new(...)`. Then `nr_clone = Arc::clone(&nr)` at `pipeline.rs:450`. `nr_clone` is moved into the worker thread. After the worker thread is joined (either at `pipeline.rs:531` when relations start, or at `pipeline.rs:578` after the PBF loop), `nr_clone`'s Arc is dropped. But `nr` itself — where does it live?

`nr` is a local in the `if block_tx.is_none()` block which is inside the `BlockType::Ways` arm. The block ends at `pipeline.rs:518` with the closing `}`. So `nr` drops at line 518. But `nr_clone` (a clone of the Arc) was moved into the worker thread closure. When the worker joins, the closure is consumed and `nr_clone` drops.

So after worker join at `pipeline.rs:531`:
- `nr` was dropped at `pipeline.rs:518` (end of if block).
- `nr_clone` was dropped when the worker thread closure completed.
- The `SortedNodeStoreReader` inside the Arc is freed.

**CORRECTION**: The NodeStoreReader IS dropped when the worker thread joins, which happens before relation processing. The `nr` Arc drops at end of the `if block_tx.is_none()` block (line 518), and `nr_clone` drops when worker thread closure completes (joined at line 531). So by line 540, when relation processing starts, the NodeStoreReader is already freed.

**Wait — is that true?** Let me trace more carefully:

1. `nr` created at `pipeline.rs:443` inside `if block_tx.is_none() { ... }` (lines 440-518).
2. `nr_clone = Arc::clone(&nr)` at `pipeline.rs:450`.
3. `nr_clone` moved into worker thread closure at `pipeline.rs:458`.
4. `nr` — its scope ends at `pipeline.rs:518` (closing brace of the `if block_tx.is_none()` block). **But this block is only entered once** (when `block_tx.is_none()`, i.e., first way block).
5. After this block: `block_tx = Some(btx)` at `pipeline.rs:517`.
6. On the **very next** way block iteration, `block_tx.is_some()`, so the if block is skipped.
7. `nr` has already been dropped at end of the if block.

So `nr` drops immediately after the first way block is processed (end of if block at line 518). The only remaining reference is `nr_clone` inside the worker thread.

When relations start (`pipeline.rs:528-539`):
- `block_tx.take()` → dropped → worker thread's `brx.recv()` returns Err → worker finishes `rayon::in_place_scope` → `nr_clone` drops → worker thread ends.
- `worker_handle.take().join()` at `pipeline.rs:531` — waits for worker to finish.
- After join: `nr_clone` has been dropped. The SortedNodeStoreReader's Arc strong count was 2 (nr + nr_clone). After both drops, refcount = 0, reader is freed.

**VERIFIED: The NodeStoreReader IS correctly dropped before relation processing begins.** My initial finding was wrong. The pipeline already releases the NodeStore memory before relations run.

### 2. SortWriter buffer_bytes underestimates actual memory

Covered above. The 16-byte-per-record undercount means actual chunk sizes in memory are ~15-25% larger than the 1 GB target.

### 3. No phase overlap opportunities

**Ocean depends on data_bounds** (computed from all nodes), so it can't overlap with PBF.
**Sort depends on all chunks being finalized**, so it can't overlap with ocean.
**Assembly depends on sorted merge**, so it can't overlap with sort.

The only potential overlap: ocean processing with relation processing (they don't share data). But both write to the same SortWriter, which is not thread-safe.

### 4. data_bounds Filter Interaction

`data_bounds` is computed from min/max lat/lon of all nodes (`pipeline.rs:602-616`). It adds a ~1 degree buffer. This is used by ocean processing to filter shapefile records — polygons outside data_bounds are skipped.

For Denmark, this means most of the planet's ocean polygons are skipped. For planet, data_bounds covers the whole world, so all ocean polygons are processed.

The data_bounds also applies to ocean polygon pre-clipping (`ocean.rs:83-86, 167`): polygons are clipped to data_bounds before being added to the polygon list. This is especially effective for partial extracts.

### 5. Empty Dataset Edge Cases

If the PBF has no nodes, `min_lat_e7 > max_lat_e7` stays true, so `data_bounds` defaults to full world (`pipeline.rs:614-616`). All ocean polygons would be processed. Sort would have no PBF records but ocean records would exist.

If there are no features at all (no matching tags), sort has empty chunks. SortReader gets an empty heap. Assembly produces an empty PMTiles file.

### 6. Race Conditions

**Way phase drain thread**: Owns `way_index` + `sort_writer` exclusively. Main thread only sends blocks. Worker thread only sends results. No shared mutable state beyond the channels.

**Ocean rayon workers**: Each worker has its own `OceanAcc` buffer. Chunk IDs allocated via `AtomicUsize` — no races. Sort chunk files have unique names by ID.

**LandMask**: Uses `AtomicU8` for thread-safe bit setting (`geometry.rs:1213`). No races.

**Assembly phase**: Reader, encoder, writer communicate only through channels. PMTiles writer is owned exclusively by the writer thread.

**No race conditions identified.**

### 7. Could Sort Phase Start Flushing While PBF Is Still Running?

The sort phase (external merge) requires ALL chunks to be finalized. During PBF processing, chunks are being flushed incrementally by the SortWriter. But the merge can't start until `finish()` is called.

A streaming merge (that accepts new chunks dynamically) would be possible but complex. Given sort is only ~0.5s for Denmark and likely <5s for planet, the win is minimal.

### 8. PBF Reader Blocks During Way Phase

The main thread iterates `reader.into_blocks_pipelined()` at `pipeline.rs:417`. During the way sub-phase, the main thread sends each block to the worker thread via `block_tx.send()` at `pipeline.rs:521-522`. This is a `sync_channel(1)`, so the main thread blocks if the worker hasn't consumed the previous block.

Meanwhile, pbfhogg's pipelined reader has its own internal buffering. If the main thread is blocked on send, pbfhogg's I/O and decode threads continue filling their buffers until they're full. The decode thread count (`threads/3`) determines how many blocks can be decoded ahead.

**Potential issue**: If the worker thread is slow (rayon pool saturated), the main thread blocks, which eventually backpressures to pbfhogg. This is intentional (memory control) but could cause I/O stalls on fast storage.

---

## Summary of Findings

### Verified Claims from Theoretical Review

1. **HIGH: Assemble pipeline sync_channel(1) underlap** — Verified. Two sync_channel(1) at `pipeline.rs:1342-1343`. Single writer stall propagates to all threads. Depth 2-4 would provide buffering.

2. **MEDIUM: Static decode thread split** — Verified. `threads/3` at `pipeline.rs:327`. Oversubscription: total thread count exceeds CPU count during PBF phase. No per-phase tuning.

3. **MEDIUM: Fixed 1 GB sort chunk** — Verified. `pipeline.rs:112`. buffer_bytes underestimates by ~15-25%. Not adaptive to available memory.

### New Findings

4. **LOW: NodeStoreReader correctly dropped before relations** — Initially suspected excessive lifetime, but code analysis shows the Arc is correctly dropped when the worker thread joins before relation processing. No action needed.

5. **LOW: SortWriter buffer_bytes underestimates memory** — Each SortRecord is 32 bytes but only `data.len() + 8` counted. ~15-25% undercount means actual 1 GB chunks use ~1.2 GB.

6. **LOW: No data integrity check for --skip-to** — Reused chunk files are assumed valid. No hash/timestamp verification against the PBF file. Stale data risk if PBF changed.

7. **LOW: LandMask persists through sort and assembly** — 32 MB, negligible at planet scale but technically unnecessary after ocean phase.

8. **MEDIUM: No phase overlap possible** — Ocean depends on data_bounds from PBF. Sort depends on all chunks. Assembly depends on merge. The pipeline is inherently sequential. The only possible overlap (ocean + relations) requires a redesign of SortWriter ownership.

9. **INFO: Planet-scale memory budget** — NodeStore (52 GB) + WayIndex (~10 GB) coexist during the way sub-phase. After way phase: NodeStore dropped (line 518 + worker join), WayIndex dropped (line 596). Before ocean starts, memory is down to: SortWriter buffer (~1 GB) + LandMask (32 MB) + sort chunks on disk. Plenty of headroom for ocean processing.

### Critical Path for Planet Scale

The memory timeline for planet scale shows the critical bottleneck is during the **way sub-phase** of Phase 1+2:
- SortedNodeStore: 44-52 GB
- WayIndex: ~10 GB (grows during ways)
- SortWriter buffer: ~1 GB
- LandMask: 32 MB
- Rayon per-thread state: ~few MB
- **Total: 55-63 GB on a 64 GB machine**

This is the finding from Box 2 that dominates all other concerns. The pipeline orchestration itself is sound — the issue is that these two large structures must coexist during way processing because ways need both node coordinates (NodeStore) and way geometry recording (WayIndex).
